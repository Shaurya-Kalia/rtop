use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, HighlightSpacing, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, StatefulWidget, Table, TableState, Widget,
    },
    Frame, Terminal,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::CString,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use sysinfo::{
    CpuRefreshKind, Disks, MemoryRefreshKind, Networks, ProcessRefreshKind, ProcessesToUpdate,
    System, UpdateKind,
};

const HISTORY_LEN: usize = 160;

#[derive(PartialEq, Clone, Copy)]
enum FocusPanel {
    Processes,
    Disks,
}

/// Custom Block-Style Graph with vertical RGB Gradient toward White (255, 255, 255).
/// Supports normal (Bottom-Up) and inverted (Top-Down) growth.
struct BlockGraph<'a> {
    data: &'a VecDeque<u64>,
    max: u64,
    inverted: bool,
    base_rgb: (u8, u8, u8),
    label: Option<String>,
}

impl<'a> BlockGraph<'a> {
    fn new(data: &'a VecDeque<u64>, max: u64, inverted: bool, base_rgb: (u8, u8, u8)) -> Self {
        Self {
            data,
            max: max.max(1),
            inverted,
            base_rgb,
            label: None,
        }
    }

    fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Interpolates from `base_rgb` at `row = 0` (baseline) to pure White `(255, 255, 255)`
    /// at the furthest row (`height - 1`).
    fn row_gradient_color(&self, row: u16, total_height: u16) -> Color {
        let t = if total_height <= 1 {
            0.0
        } else {
            (row as f32) / ((total_height - 1) as f32)
        };
        let (r0, g0, b0) = self.base_rgb;
        let r = (r0 as f32 + (255.0 - r0 as f32) * t).round() as u8;
        let g = (g0 as f32 + (255.0 - g0 as f32) * t).round() as u8;
        let b = (b0 as f32 + (255.0 - b0 as f32) * t).round() as u8;
        Color::Rgb(r, g, b)
    }
}

impl<'a> Widget for BlockGraph<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let width = area.width as usize;
        let height_sub = (area.height as u64) * 2;
        let bg_empty = Style::default().fg(Color::Rgb(38, 42, 50));

        let start_idx = self.data.len().saturating_sub(width);
        let slice: Vec<u64> = self.data.iter().skip(start_idx).copied().collect();
        let x_offset = width.saturating_sub(slice.len());

        for y in 0..area.height {
            for x in 0..area.width {
                buf[(area.x + x, area.y + y)]
                .set_char('·')
                .set_style(bg_empty);
            }
        }

        for (i, &val) in slice.iter().enumerate() {
            let x = area.x + (x_offset + i) as u16;
            let clamped = val.min(self.max);
            let mut filled_sub = (clamped * height_sub) / self.max;
            if clamped > 0 && filled_sub == 0 {
                filled_sub = 1;
            }

            for row in 0..area.height {
                let y = if self.inverted {
                    area.y + row
                } else {
                    area.y + area.height - 1 - row
                };

                let row_style = Style::default().fg(self.row_gradient_color(row, area.height));
                let row_sub_start = (row as u64) * 2;

                if filled_sub >= row_sub_start + 2 {
                    buf[(x, y)].set_char('█').set_style(row_style);
                } else if filled_sub == row_sub_start + 1 {
                    let ch = if self.inverted { '▀' } else { '▄' };
                    buf[(x, y)].set_char(ch).set_style(row_style);
                }
            }
        }

        if let Some(lbl) = self.label {
            let label_y = if self.inverted {
                area.y + area.height.saturating_sub(1)
            } else {
                area.y
            };
            let badge = format!(" {} ", lbl);
            for (i, ch) in badge.chars().enumerate() {
                if (i as u16) < area.width {
                    buf[(area.x + 1 + i as u16, label_y)].set_char(ch).set_style(
                        Style::default()
                        .fg(Color::White)
                        .bg(Color::Rgb(30, 34, 42))
                        .add_modifier(Modifier::BOLD),
                    );
                }
            }
        }
    }
}

/// Vertically centered meter bar:
/// Uses '■' (U+25A0, centered medium block) for filled and '▪' (U+25AA, centered small square)
/// so the bar sits symmetrically on the text midline with equal top and bottom margin.
fn make_block_bar(pct: u64, width: usize) -> (String, String) {
    let filled = ((pct.min(100) as usize) * width) / 100;
    let empty = width.saturating_sub(filled);
    ("■".repeat(filled), "▪".repeat(empty))
}

#[derive(Default, Clone)]
struct CpuTelemetry {
    hwmon_path: Option<PathBuf>,
    rapl_path: Option<PathBuf>,
    prev_rapl_uj: u64,
    last_rapl_time: Option<Instant>,
    package_temp_c: u64,
    core_temps_c: Vec<u64>,
    package_power_w: f64,
}

impl CpuTelemetry {
    fn new() -> Self {
        let mut t = Self::default();
        if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = read_sysfs_string(&path.join("name")).trim().to_string();
                if matches!(name.as_str(), "k10temp" | "zenpower" | "coretemp") {
                    t.hwmon_path = Some(path);
                    break;
                }
            }
        }
        let rapl = PathBuf::from("/sys/class/powercap/intel-rapl:0/energy_uj");
        if rapl.exists() {
            t.rapl_path = Some(rapl);
        } else {
            let rapl_alt = PathBuf::from("/sys/class/powercap/intel-rapl/intel-rapl:0/energy_uj");
            if rapl_alt.exists() {
                t.rapl_path = Some(rapl_alt);
            }
        }
        t
    }

    fn update(&mut self) {
        if let Some(ref hwmon) = self.hwmon_path {
            self.package_temp_c = read_sysfs_u64(&hwmon.join("temp1_input")) / 1000;
            let mut c_temps = Vec::new();
            for idx in 2..=32 {
                let p = hwmon.join(format!("temp{}_input", idx));
                if p.exists() {
                    let val = read_sysfs_u64(&p) / 1000;
                    if val > 0 && val < 130 {
                        c_temps.push(val);
                    }
                }
            }
            self.core_temps_c = c_temps;

            let p_avg = read_sysfs_u64(&hwmon.join("power1_average"));
            let p_in = read_sysfs_u64(&hwmon.join("power1_input"));
            let uw = if p_avg > 0 { p_avg } else { p_in };
            if uw > 0 {
                self.package_power_w = uw as f64 / 1_000_000.0;
            }
        }

        if let Some(ref rapl) = self.rapl_path {
            let now = Instant::now();
            let cur_uj = read_sysfs_u64(rapl);
            if let Some(prev_t) = self.last_rapl_time {
                let dt_s = now.duration_since(prev_t).as_secs_f64();
                if dt_s > 0.0 && cur_uj >= self.prev_rapl_uj && self.prev_rapl_uj > 0 {
                    self.package_power_w =
                    ((cur_uj - self.prev_rapl_uj) as f64 / 1_000_000.0) / dt_s;
                }
            }
            self.prev_rapl_uj = cur_uj;
            self.last_rapl_time = Some(now);
        }
    }
}


#[derive(Default, Clone)]
struct AmdGpuStats {
    card_path: Option<PathBuf>,
    hwmon_path: Option<PathBuf>,
    gfx_pct: u64,
    vcn_dec_pct: u64,
    vcn_enc_pct: u64,
    vram_used_mb: u64,
    vram_total_mb: u64,
    gtt_used_mb: u64,
    gtt_total_mb: u64,
    temp_edge_c: u64,
    temp_junc_c: u64,
    power_w: f64,
    sclk_mhz: u64,
    mclk_mhz: u64,
    fan_rpm: u64,
    prev_dec_ns: u64,
    prev_enc_ns: u64,
    prev_pid_gpu_ns: HashMap<u32, u64>,
    pid_gpu_pct: HashMap<u32, f32>,
    // Cache of (pid, fdinfo_path) that actually belong to amdgpu, refreshed every 10 ticks
    cached_drm_fds: Vec<(u32, PathBuf)>,
    tick_counter: u32,
    last_sample: Option<Instant>,
}

impl AmdGpuStats {
    fn new() -> Self {
        let mut stats = Self::default();
        if let Ok(entries) = fs::read_dir("/sys/class/drm") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("card") && !name.contains('-') {
                    let dev_path = entry.path().join("device");
                    let vendor = read_sysfs_string(&dev_path.join("vendor"));
                    if vendor.trim() == "0x1002" {
                        stats.card_path = Some(dev_path.clone());
                        if let Ok(hwmons) = fs::read_dir(dev_path.join("hwmon")) {
                            if let Some(hwmon) = hwmons.flatten().next() {
                                stats.hwmon_path = Some(hwmon.path());
                            }
                        }
                        break;
                    }
                }
            }
        }
        stats.last_sample = Some(Instant::now());
        stats
    }

    fn update(&mut self) {
        let now = Instant::now();
        let dt_ns = self
        .last_sample
        .map(|t| now.duration_since(t).as_nanos() as u64)
        .unwrap_or(500_000_000)
        .max(1);
        self.last_sample = Some(now);

        if let Some(ref dev) = self.card_path {
            self.gfx_pct = read_sysfs_u64(&dev.join("gpu_busy_percent"));
            self.vram_used_mb = read_sysfs_u64(&dev.join("mem_info_vram_used")) / (1024 * 1024);
            self.vram_total_mb = read_sysfs_u64(&dev.join("mem_info_vram_total")) / (1024 * 1024);
            self.gtt_used_mb = read_sysfs_u64(&dev.join("mem_info_gtt_used")) / (1024 * 1024);
            self.gtt_total_mb = read_sysfs_u64(&dev.join("mem_info_gtt_total")) / (1024 * 1024);
        }

        if let Some(ref hwmon) = self.hwmon_path {
            self.temp_edge_c = read_sysfs_u64(&hwmon.join("temp1_input")) / 1000;
            self.temp_junc_c = read_sysfs_u64(&hwmon.join("temp2_input")) / 1000;
            let uw = {
                let avg = read_sysfs_u64(&hwmon.join("power1_average"));
                if avg > 0 {
                    avg
                } else {
                    read_sysfs_u64(&hwmon.join("power1_input"))
                }
            };
            self.power_w = uw as f64 / 1_000_000.0;
            self.sclk_mhz = read_sysfs_u64(&hwmon.join("freq1_input")) / 1_000_000;
            self.mclk_mhz = read_sysfs_u64(&hwmon.join("freq2_input")) / 1_000_000;
            self.fan_rpm = read_sysfs_u64(&hwmon.join("fan1_input"));
        }

        // Re-discover open /dev/dri/* file descriptors only once every 10 ticks (5 seconds)
        if self.tick_counter % 10 == 0 || self.cached_drm_fds.is_empty() {
            self.cached_drm_fds = discover_amd_drm_fds();
        }
        self.tick_counter = self.tick_counter.wrapping_add(1);

        let (cur_dec_ns, cur_enc_ns, cur_pid_ns) = sample_cached_amd_fdinfo(&mut self.cached_drm_fds);
        if self.prev_dec_ns > 0 && cur_dec_ns >= self.prev_dec_ns {
            let delta_dec = cur_dec_ns - self.prev_dec_ns;
            self.vcn_dec_pct = ((delta_dec * 100) / dt_ns).min(100);
        } else if let Some(ref dev) = self.card_path {
            self.vcn_dec_pct = read_sysfs_u64(&dev.join("vcn_busy_percent"));
        }

        if self.prev_enc_ns > 0 && cur_enc_ns >= self.prev_enc_ns {
            let delta_enc = cur_enc_ns - self.prev_enc_ns;
            self.vcn_enc_pct = ((delta_enc * 100) / dt_ns).min(100);
        }

        let mut new_pid_pct = HashMap::new();
        for (&pid, &total_ns) in &cur_pid_ns {
            if let Some(&prev_ns) = self.prev_pid_gpu_ns.get(&pid) {
                if total_ns >= prev_ns {
                    let delta = total_ns - prev_ns;
                    let pct = ((delta as f64 * 100.0) / (dt_ns as f64)).min(100.0) as f32;
                    if pct > 0.05 {
                        new_pid_pct.insert(pid, pct);
                    }
                }
            }
        }

        self.prev_dec_ns = cur_dec_ns;
        self.prev_enc_ns = cur_enc_ns;
        self.prev_pid_gpu_ns = cur_pid_ns;
        self.pid_gpu_pct = new_pid_pct;
    }
}

/// Runs once every 5 seconds to find which (pid, fdinfo_path) belong to /dev/dri/*
fn discover_amd_drm_fds() -> Vec<(u32, PathBuf)> {
    let mut drm_fds = Vec::new();
    let Ok(proc_entries) = fs::read_dir("/proc") else {
        return drm_fds;
    };

    for entry in proc_entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let pid_path = entry.path();
        let fd_dir = pid_path.join("fd");
        let fdinfo_dir = pid_path.join("fdinfo");

        let Ok(fds) = fs::read_dir(&fd_dir) else {
            continue;
        };

        for fd in fds.flatten() {
            if let Ok(target) = fs::read_link(fd.path()) {
                if target.to_string_lossy().starts_with("/dev/dri/") {
                    drm_fds.push((pid, fdinfo_dir.join(fd.file_name())));
                }
            }
        }
    }
    drm_fds
}

/// Fast path: only reads the ~15-30 cached fdinfo files on each 500ms tick!
fn sample_cached_amd_fdinfo(cached_fds: &mut Vec<(u32, PathBuf)>) -> (u64, u64, HashMap<u32, u64>) {
    let mut client_vcn: HashMap<String, (u64, u64)> = HashMap::new();
    let mut pid_client_total: HashMap<(u32, String), u64> = HashMap::new();

    cached_fds.retain(|&(pid, ref fdinfo_path)| {
        let Ok(content) = fs::read_to_string(fdinfo_path) else {
            return false; // Drop closed FD from cache immediately
        };
        if !content.contains("drm-driver:\tamdgpu") {
            return false;
        }

        let mut client_id = String::new();
        let mut dec_ns = 0u64;
        let mut enc_ns = 0u64;
        let mut all_engines_ns = 0u64;

        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("drm-client-id:\t") {
                client_id = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("drm-engine-") {
                if let Some((engine_name, val_str)) = rest.split_once(":\t") {
                    let ns = parse_fdinfo_ns(val_str);
                    all_engines_ns += ns;
                    if engine_name.contains("dec") {
                        dec_ns += ns;
                    } else if engine_name.contains("enc") || engine_name.contains("vpe") {
                        enc_ns += ns;
                    }
                }
            }
        }

        if !client_id.is_empty() {
            if dec_ns > 0 || enc_ns > 0 {
                client_vcn.insert(client_id.clone(), (dec_ns, enc_ns));
            }
            if all_engines_ns > 0 {
                pid_client_total.insert((pid, client_id), all_engines_ns);
            }
        }
        true
    });

    let total_dec: u64 = client_vcn.values().map(|(d, _)| *d).sum();
    let total_enc: u64 = client_vcn.values().map(|(_, e)| *e).sum();

    let mut pid_totals: HashMap<u32, u64> = HashMap::new();
    for ((pid, _), ns) in pid_client_total {
        *pid_totals.entry(pid).or_insert(0) += ns;
    }

    (total_dec, total_enc, pid_totals)
}

fn parse_fdinfo_ns(val: &str) -> u64 {
    val.split_whitespace()
    .next()
    .and_then(|s| s.parse::<u64>().ok())
    .unwrap_or(0)
}

fn read_sysfs_string(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn read_sysfs_u64(path: &Path) -> u64 {
    fs::read_to_string(path)
    .unwrap_or_default()
    .trim()
    .parse::<u64>()
    .unwrap_or(0)
}

fn read_cached_memory_bytes() -> u64 {
    let Ok(content) = fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    let mut cached_kb = 0u64;
    let mut sreclaim_kb = 0u64;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("Cached:") {
            cached_kb = parse_fdinfo_ns(rest.trim());
        } else if let Some(rest) = line.strip_prefix("SReclaimable:") {
            sreclaim_kb = parse_fdinfo_ns(rest.trim());
        }
    }
    (cached_kb + sreclaim_kb) * 1024
}

#[derive(Clone)]
struct DiskEntry {
    label: String,
    total_gb: f64,
    used_gb: f64,
    avail_gb: f64,
    read_mb_s: f64,
    write_mb_s: f64,
}

fn statvfs_path(path: &str) -> Option<(u64, u64, String)> {
    let c_path = CString::new(path).ok()?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
            let total = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
            let avail = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
            if total > 0 {
                return Some((total, avail, String::new()));
            }
        }
    }
    None
}

fn read_diskstats_sectors() -> HashMap<String, (u64, u64)> {
    let mut map = HashMap::new();
    let Ok(content) = fs::read_to_string("/proc/diskstats") else {
        return map;
    };
    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 10 {
            let dev_name = parts[2].to_string();
            let sectors_read = parts[5].parse::<u64>().unwrap_or(0);
            let sectors_written = parts[9].parse::<u64>().unwrap_or(0);
            map.insert(dev_name, (sectors_read, sectors_written));
        }
    }
    map
}

struct App {
    start_time: Instant,
    sys: System,
    disks: Disks,
    networks: Networks,
    cpu_telemetry: CpuTelemetry,
    gpu: AmdGpuStats,
    cached_mem_bytes: u64,
    cpu_history: VecDeque<u64>,
    gpu_history: VecDeque<u64>,
    net_rx_history: VecDeque<u64>,
    net_tx_history: VecDeque<u64>,
    peak_rx_kib: u64,
    peak_tx_kib: u64,
    prev_disk_sectors: HashMap<String, (u64, u64)>,
    last_disk_time: Instant,
    disk_entries: Vec<DiskEntry>,
    focus: FocusPanel,
    proc_state: TableState,
    disk_scroll: usize,
}

impl App {
    fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        let mut proc_state = TableState::default();
        proc_state.select(Some(0));

        let mut app = Self {
            start_time: Instant::now(),
            sys,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            cpu_telemetry: CpuTelemetry::new(),
            gpu: AmdGpuStats::new(),
            cached_mem_bytes: read_cached_memory_bytes(),
            cpu_history: VecDeque::from(vec![0; HISTORY_LEN]),
            gpu_history: VecDeque::from(vec![0; HISTORY_LEN]),
            net_rx_history: VecDeque::from(vec![0; HISTORY_LEN]),
            net_tx_history: VecDeque::from(vec![0; HISTORY_LEN]),
            peak_rx_kib: 1024,
            peak_tx_kib: 1024,
            prev_disk_sectors: read_diskstats_sectors(),
            last_disk_time: Instant::now(),
            disk_entries: Vec::new(),
            focus: FocusPanel::Processes,
            proc_state,
            disk_scroll: 0,
        };
        app.refresh_disk_entries();
        app
    }

    fn refresh_disk_entries(&mut self) {
        let now = Instant::now();
        let dt_s = now.duration_since(self.last_disk_time).as_secs_f64().max(0.1);
        self.last_disk_time = now;

        let cur_sectors = read_diskstats_sectors();
        let mut dev_io_mb: HashMap<String, (f64, f64)> = HashMap::new();
        for (dev, &(r_sec, w_sec)) in &cur_sectors {
            if let Some(&(pr, pw)) = self.prev_disk_sectors.get(dev) {
                let r_mb = ((r_sec.saturating_sub(pr) * 512) as f64 / 1_048_576.0) / dt_s;
                let w_mb = ((w_sec.saturating_sub(pw) * 512) as f64 / 1_048_576.0) / dt_s;
                dev_io_mb.insert(dev.clone(), (r_mb, w_mb));
            }
        }
        self.prev_disk_sectors = cur_sectors;

        let mut mount_to_dev: HashMap<String, String> = HashMap::new();
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && parts[0].starts_with("/dev/") {
                    let short_dev = parts[0].trim_start_matches("/dev/").to_string();
                    mount_to_dev.insert(parts[1].to_string(), short_dev);
                }
            }
        }

        let root_dev = mount_to_dev.get("/").cloned().unwrap_or_default();
        let mut entries = Vec::new();
        let mut seen_mounts = HashSet::new();

        let pinned = [
            ("/", "root (/)"),
            ("/home", "/home"),
            ("/tmp", "/tmp"),
            ("swap", "swap"),
            ("/var/log", "log (/var/log)"),
        ];

        for (mount_path, display_name) in pinned {
            if mount_path == "swap" {
                let total_swap = self.sys.total_swap() as f64 / 1_073_741_824.0;
                let used_swap = self.sys.used_swap() as f64 / 1_073_741_824.0;
                let avail_swap = (total_swap - used_swap).max(0.0);
                entries.push(DiskEntry {
                    label: display_name.to_string(),
                             total_gb: total_swap,
                             used_gb: used_swap,
                             avail_gb: avail_swap,
                             read_mb_s: 0.0,
                             write_mb_s: 0.0,
                });
                continue;
            }

            seen_mounts.insert(mount_path.to_string());
            if let Some((total_b, avail_b, _)) = statvfs_path(mount_path) {
                let total_gb = total_b as f64 / 1_073_741_824.0;
                let avail_gb = avail_b as f64 / 1_073_741_824.0;
                let used_gb = (total_gb - avail_gb).max(0.0);
                let dev = mount_to_dev
                .get(mount_path)
                .cloned()
                .unwrap_or_else(|| root_dev.clone());
                let (r_mb, w_mb) = dev_io_mb.get(&dev).copied().unwrap_or((0.0, 0.0));
                entries.push(DiskEntry {
                    label: display_name.to_string(),
                             total_gb,
                             used_gb,
                             avail_gb,
                             read_mb_s: r_mb,
                             write_mb_s: w_mb,
                });
            }
        }

        for disk in self.disks.list() {
            let mount_str = disk.mount_point().to_string_lossy().to_string();
            if seen_mounts.contains(&mount_str)
                || mount_str.starts_with("/boot")
                || mount_str.starts_with("/run")
                || mount_str.starts_with("/sys")
                || mount_str.starts_with("/proc")
                || mount_str.starts_with("/dev")
                {
                    continue;
                }
                seen_mounts.insert(mount_str.clone());
            let total_gb = disk.total_space() as f64 / 1_073_741_824.0;
            if total_gb < 0.1 {
                continue;
            }
            let avail_gb = disk.available_space() as f64 / 1_073_741_824.0;
            let used_gb = (total_gb - avail_gb).max(0.0);
            let dev = mount_to_dev
            .get(&mount_str)
            .cloned()
            .unwrap_or_else(|| root_dev.clone());
            let (r_mb, w_mb) = dev_io_mb.get(&dev).copied().unwrap_or((0.0, 0.0));

            entries.push(DiskEntry {
                label: mount_str,
                total_gb,
                used_gb,
                avail_gb,
                read_mb_s: r_mb,
                write_mb_s: w_mb,
            });
        }

        self.disk_entries = entries;
    }

    fn on_tick(&mut self) {
        self.sys.refresh_cpu_specifics(CpuRefreshKind::everything());
        self.sys.refresh_memory_specifics(MemoryRefreshKind::everything());
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cpu()
            .with_memory()
            .with_cmd(UpdateKind::OnlyIfNotSet),
        );
        self.disks.refresh(true);
        self.networks.refresh(true);
        self.cpu_telemetry.update();
        self.gpu.update();
        self.cached_mem_bytes = read_cached_memory_bytes();
        self.refresh_disk_entries();

        let avg_cpu = self.sys.global_cpu_usage() as u64;
        self.cpu_history.pop_front();
        self.cpu_history.push_back(avg_cpu);

        let avg_gpu = self
        .gpu
        .gfx_pct
        .max(self.gpu.vcn_dec_pct)
        .max(self.gpu.vcn_enc_pct);
        self.gpu_history.pop_front();
        self.gpu_history.push_back(avg_gpu);

        let (rx, tx) = self
        .networks
        .iter()
        .fold((0u64, 0u64), |(r, t), (_, data)| {
            (r + data.received(), t + data.transmitted())
        });
        let rx_kib = (rx / 1024) * 2;
        let tx_kib = (tx / 1024) * 2;
        self.peak_rx_kib = self.peak_rx_kib.max(rx_kib).max(64);
        self.peak_tx_kib = self.peak_tx_kib.max(tx_kib).max(64);

        self.net_rx_history.pop_front();
        self.net_rx_history.push_back(rx_kib);
        self.net_tx_history.pop_front();
        self.net_tx_history.push_back(tx_kib);
    }

    fn scroll_down(&mut self) {
        match self.focus {
            FocusPanel::Processes => {
                let total = self.sys.processes().len();
                let i = self.proc_state.selected().unwrap_or(0);
                if i + 1 < total {
                    self.proc_state.select(Some(i + 1));
                }
            }
            FocusPanel::Disks => {
                if self.disk_scroll + 1 < self.disk_entries.len() {
                    self.disk_scroll += 1;
                }
            }
        }
    }

    fn scroll_up(&mut self) {
        match self.focus {
            FocusPanel::Processes => {
                let i = self.proc_state.selected().unwrap_or(0);
                self.proc_state.select(Some(i.saturating_sub(1)));
            }
            FocusPanel::Disks => {
                self.disk_scroll = self.disk_scroll.saturating_sub(1);
            }
        }
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

fn draw_ui(f: &mut Frame, app: &mut App) {
    let root_chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
    .split(f.area());

    draw_top_panel(f, app, root_chunks[0]);
    draw_bottom_panel(f, app, root_chunks[1]);
}

fn draw_top_panel(f: &mut Frame, app: &App, area: Rect) {
    let runtime_str = format_duration(app.start_time.elapsed());
    let top_block = Block::default()
    .title(Line::from(vec![
        Span::styled(
            " rtop ",
            Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        ),
        Span::raw("── "),
                      Span::styled(
                          format!("run time: {} ", runtime_str),
                              Style::default().fg(Color::Cyan),
                      ),
                      Span::raw("── "),
                      Span::styled("poll: 500ms ", Style::default().fg(Color::Yellow)),
                      Span::raw("── "),
                      Span::styled(
                          "(Tab: switch panel | ↑/↓: scroll | q: quit) ",
                                   Style::default().fg(Color::Gray),
                      ),
    ]))
    .borders(Borders::ALL)
    .border_set(symbols::border::ROUNDED)
    .border_style(Style::default().fg(Color::DarkGray));

    let inner = top_block.inner(area);
    f.render_widget(top_block, area);

    let horiz = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
    .split(inner);

    let graph_chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Percentage(49),
                 Constraint::Length(1),
                 Constraint::Percentage(50),
    ])
    .split(horiz[0]);

    // CPU Graph: Bottom-up gradient from Green (46, 204, 113) -> White (255, 255, 255)
    let cur_cpu = app.cpu_history.back().copied().unwrap_or(0);
    let cpu_graph = BlockGraph::new(&app.cpu_history, 100, false, (46, 204, 113))
    .label(format!("CPU Usage: {:>3}%", cur_cpu));
    f.render_widget(cpu_graph, graph_chunks[0]);

    let divider_str = "─".repeat(graph_chunks[1].width as usize);
    f.render_widget(
        Paragraph::new(Span::styled(
            divider_str,
            Style::default().fg(Color::DarkGray),
        )),
        graph_chunks[1],
    );

    // GPU Graph: Top-down gradient from Lime Green (80, 220, 100) -> White (255, 255, 255)
    let cur_gpu = app.gpu_history.back().copied().unwrap_or(0);
    let gpu_graph = BlockGraph::new(&app.gpu_history, 100, true, (80, 220, 100))
    .label(format!("GPU Usage: {:>3}%", cur_gpu));
    f.render_widget(gpu_graph, graph_chunks[2]);

    let right_chunks = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Percentage(55),
                 Constraint::Length(1),
                 Constraint::Percentage(44),
    ])
    .split(horiz[1]);

    draw_cpu_core_box(f, app, right_chunks[0]);
    draw_amd_gpu_box(f, app, right_chunks[2]);
}

fn temp_color(temp_c: u64) -> Color {
    if temp_c >= 85 {
        Color::LightRed
    } else if temp_c >= 70 {
        Color::Yellow
    } else {
        Color::Cyan
    }
}

fn draw_cpu_core_box(f: &mut Frame, app: &App, area: Rect) {
    let cpu_brand = app
    .sys
    .cpus()
    .first()
    .map(|c| c.brand().to_string())
    .unwrap_or_else(|| "CPU".to_string());

    let cpus = app.sys.cpus();
    let avg_freq_ghz = if !cpus.is_empty() {
        cpus.iter().map(|c| c.frequency() as f64).sum::<f64>() / (cpus.len() as f64 * 1000.0)
    } else {
        0.0
    };

    let pkg_temp = app.cpu_telemetry.package_temp_c;
    let power_w = app.cpu_telemetry.package_power_w;

    let title = format!(
        " {} ── {:.2} GHz ── {}°C ── {:.1}W ",
        cpu_brand.trim(),
                        avg_freq_ghz,
                        pkg_temp,
                        power_w
    );

    let block = Block::default()
    .title(Span::styled(
        title,
        Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD),
    ))
    .borders(Borders::ALL)
    .border_set(symbols::border::ROUNDED)
    .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    let total_pct = app.sys.global_cpu_usage() as u64;
    let (f_bar, e_bar) = make_block_bar(total_pct, 28);

    lines.push(Line::from(vec![
        Span::styled(
            "CPU ",
            Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(f_bar, Style::default().fg(Color::Green)),
                          Span::styled(e_bar, Style::default().fg(Color::DarkGray)),
                          Span::styled(
                              format!(" {:>3}%   ", total_pct),
                                  Style::default().fg(Color::LightGreen),
                          ),
                          Span::styled("Pkg Temp: ", Style::default().fg(Color::Gray)),
                          Span::styled(
                              format!("{:>2}°C", pkg_temp),
                                  Style::default()
                                  .fg(temp_color(pkg_temp))
                                  .add_modifier(Modifier::BOLD),
                          ),
    ]));

    let half = (cpus.len() + 1) / 2;
    let max_core_rows = inner.height.saturating_sub(2) as usize;

    for i in 0..half.min(max_core_rows) {
        let mut row_spans = format_core_column(i, &cpus[i], &app.cpu_telemetry);
        row_spans.push(Span::raw("   "));
        if i + half < cpus.len() {
            let right_idx = i + half;
            row_spans.extend(format_core_column(
                right_idx,
                &cpus[right_idx],
                &app.cpu_telemetry,
            ));
        }
        lines.push(Line::from(row_spans));
    }

    let load = System::load_average();
    lines.push(Line::from(vec![
        Span::styled("Load avg: ", Style::default().fg(Color::Gray)),
                          Span::styled(
                              format!("{:.2}  {:.2}  {:.2}", load.one, load.five, load.fifteen),
                                  Style::default().fg(Color::Cyan),
                          ),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

fn format_core_column(
    idx: usize,
    cpu: &sysinfo::Cpu,
    telemetry: &CpuTelemetry,
) -> Vec<Span<'static>> {
    let usage = cpu.cpu_usage() as u64;
    let freq_ghz = cpu.frequency() as f64 / 1000.0;
    let freq_pct = ((freq_ghz / 5.7) * 100.0).min(100.0) as u64;
    let core_temp = telemetry
    .core_temps_c
    .get(idx)
    .copied()
    .unwrap_or(telemetry.package_temp_c);

    let (u_fill, u_emp) = make_block_bar(usage, 7);
    let (f_fill, f_emp) = make_block_bar(freq_pct, 7);

    vec![
        Span::styled(format!("C{:<2} ", idx), Style::default().fg(Color::White)),
        Span::styled(u_fill, Style::default().fg(Color::Green)),
        Span::styled(u_emp, Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {:>3}% ", usage),
                Style::default().fg(Color::LightGreen),
        ),
        Span::styled(f_fill, Style::default().fg(Color::Magenta)),
        Span::styled(f_emp, Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {:.1}G ", freq_ghz),
                Style::default().fg(Color::LightMagenta),
        ),
        Span::styled(
            format!("{:>2}°C", core_temp),
                Style::default().fg(temp_color(core_temp)),
        ),
    ]
}

fn draw_amd_gpu_box(f: &mut Frame, app: &App, area: Rect) {
    let g = &app.gpu;
    let title = format!(
        " AMD GPU ── {}°C (Junc {}°C) ── {:.1}W ",
                        g.temp_edge_c, g.temp_junc_c, g.power_w
    );

    let block = Block::default()
    .title(Span::styled(
        title,
        Style::default()
        .fg(Color::LightGreen)
        .add_modifier(Modifier::BOLD),
    ))
    .borders(Borders::ALL)
    .border_set(symbols::border::ROUNDED)
    .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let vram_pct = if g.vram_total_mb > 0 {
        (g.vram_used_mb * 100) / g.vram_total_mb
    } else {
        0
    };
    let gtt_pct = if g.gtt_total_mb > 0 {
        (g.gtt_used_mb * 100) / g.gtt_total_mb
    } else {
        0
    };

    let (gfx_f, gfx_e) = make_block_bar(g.gfx_pct, 24);
    let (dec_f, dec_e) = make_block_bar(g.vcn_dec_pct, 24);
    let (enc_f, enc_e) = make_block_bar(g.vcn_enc_pct, 24);
    let (vram_f, vram_e) = make_block_bar(vram_pct, 12);
    let (gtt_f, gtt_e) = make_block_bar(gtt_pct, 10);

    let spacious = inner.height >= 7;
    let mut lines = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            "GFX 3D  ",
            Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(gfx_f, Style::default().fg(Color::Green)),
                          Span::styled(gfx_e, Style::default().fg(Color::DarkGray)),
                          Span::styled(
                              format!(" {:>3}%   ", g.gfx_pct),
                                  Style::default().fg(Color::LightGreen),
                          ),
                          Span::styled(
                              format!("SCLK: {:>4} MHz", g.sclk_mhz),
                                  Style::default().fg(Color::Cyan),
                          ),
    ]));
    if spacious {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled(
            "VCN DEC ",
            Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(dec_f, Style::default().fg(Color::Yellow)),
                          Span::styled(dec_e, Style::default().fg(Color::DarkGray)),
                          Span::styled(
                              format!(" {:>3}%   ", g.vcn_dec_pct),
                                  Style::default().fg(Color::Yellow),
                          ),
                          Span::styled(
                              format!("MCLK: {:>4} MHz", g.mclk_mhz),
                                  Style::default().fg(Color::Cyan),
                          ),
    ]));
    if spacious {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled(
            "VCN ENC ",
            Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(enc_f, Style::default().fg(Color::Magenta)),
                          Span::styled(enc_e, Style::default().fg(Color::DarkGray)),
                          Span::styled(
                              format!(" {:>3}%   ", g.vcn_enc_pct),
                                  Style::default().fg(Color::Magenta),
                          ),
                          Span::styled(
                              format!("FAN:  {:>4} RPM", g.fan_rpm),
                                  Style::default().fg(Color::LightBlue),
                          ),
    ]));
    if spacious {
        lines.push(Line::from(""));
    }

    lines.push(Line::from(vec![
        Span::styled("VRAM    ", Style::default().fg(Color::White)),
                          Span::styled(vram_f, Style::default().fg(Color::LightCyan)),
                          Span::styled(vram_e, Style::default().fg(Color::DarkGray)),
                          Span::raw(format!(
                              " {:.1}/{:.1}G   ",
                              g.vram_used_mb as f64 / 1024.0,
                              g.vram_total_mb as f64 / 1024.0
                          )),
                          Span::styled("GTT ", Style::default().fg(Color::White)),
                          Span::styled(gtt_f, Style::default().fg(Color::Blue)),
                          Span::styled(gtt_e, Style::default().fg(Color::DarkGray)),
                          Span::raw(format!(
                              " {:.1}/{:.1}G",
                              g.gtt_used_mb as f64 / 1024.0,
                              g.gtt_total_mb as f64 / 1024.0
                          )),
    ]));

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_bottom_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let horiz = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
    .split(area);

    let left_vert = Layout::default()
    .direction(Direction::Vertical)
    .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
    .split(horiz[0]);

    let mem_disk_split = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
    .split(left_vert[0]);

    draw_mem_box(f, app, mem_disk_split[0]);
    draw_disks_box(f, app, mem_disk_split[1]);
    draw_net_box(f, app, left_vert[1]);
    draw_proc_box(f, app, horiz[1]);
}

fn draw_mem_box(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
    .title(Span::styled(
        " mem ",
        Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD),
    ))
    .borders(Borders::ALL)
    .border_set(symbols::border::ROUNDED)
    .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let total_gb = app.sys.total_memory() as f64 / 1_073_741_824.0;
    let used_gb = app.sys.used_memory() as f64 / 1_073_741_824.0;
    let avail_gb = app.sys.available_memory() as f64 / 1_073_741_824.0;
    let cached_gb = app.cached_mem_bytes as f64 / 1_073_741_824.0;
    let free_gb = app.sys.free_memory() as f64 / 1_073_741_824.0;

    let denom = total_gb.max(0.1);
    let used_pct = ((used_gb / denom) * 100.0) as u64;
    let avail_pct = ((avail_gb / denom) * 100.0) as u64;
    let cached_pct = ((cached_gb / denom) * 100.0) as u64;
    let free_pct = ((free_gb / denom) * 100.0) as u64;

    let bar_w = inner.width.saturating_sub(6) as usize;
    let (u_f, u_e) = make_block_bar(used_pct, bar_w);
    let (a_f, a_e) = make_block_bar(avail_pct, bar_w);
    let (c_f, c_e) = make_block_bar(cached_pct, bar_w);
    let (f_f, f_e) = make_block_bar(free_pct, bar_w);

    let lines = vec![
        Line::from(vec![
            Span::styled("Total:     ", Style::default().fg(Color::Gray)),
                   Span::styled(
                       format!("{:>6.1} GiB", total_gb),
                           Style::default()
                           .fg(Color::White)
                           .add_modifier(Modifier::BOLD),
                   ),
        ]),
        Line::from(format!("Used:      {:>6.2} GiB", used_gb)),
        Line::from(vec![
            Span::styled(format!("{:>3}% ", used_pct), Style::default().fg(Color::Green)),
                   Span::styled(u_f, Style::default().fg(Color::Green)),
                   Span::styled(u_e, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(format!("Available: {:>6.1} GiB", avail_gb)),
        Line::from(vec![
            Span::styled(
                format!("{:>3}% ", avail_pct),
                    Style::default().fg(Color::Yellow),
            ),
            Span::styled(a_f, Style::default().fg(Color::Yellow)),
                   Span::styled(a_e, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(format!("Cached:    {:>6.2} GiB", cached_gb)),
        Line::from(vec![
            Span::styled(
                format!("{:>3}% ", cached_pct),
                    Style::default().fg(Color::Cyan),
            ),
            Span::styled(c_f, Style::default().fg(Color::Cyan)),
                   Span::styled(c_e, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(format!("Free:      {:>6.1} GiB", free_gb)),
        Line::from(vec![
            Span::styled(
                format!("{:>3}% ", free_pct),
                    Style::default().fg(Color::LightGreen),
            ),
            Span::styled(f_f, Style::default().fg(Color::LightGreen)),
                   Span::styled(f_e, Style::default().fg(Color::DarkGray)),
        ]),
    ];

    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_disks_box(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.focus == FocusPanel::Disks;
    let border_col = if is_focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let block = Block::default()
    .title(Line::from(vec![
        Span::styled(
            " disks & io ",
            Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("({}/{}) ", app.disk_scroll + 1, app.disk_entries.len().max(1)),
                Style::default().fg(Color::Gray),
        ),
    ]))
    .borders(Borders::ALL)
    .border_set(symbols::border::ROUNDED)
    .border_style(Style::default().fg(border_col));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = Vec::new();
    let visible_items = (inner.height as usize / 2).max(1);

    for disk in app
        .disk_entries
        .iter()
        .skip(app.disk_scroll)
        .take(visible_items)
        {
            let pct = if disk.total_gb > 0.0 {
                ((disk.used_gb / disk.total_gb) * 100.0) as u64
            } else {
                0
            };
            let (u_f, u_e) = make_block_bar(pct, 8);

            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<15}", disk.label),
                        Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("Tot:{:>4.0}G  ", disk.total_gb),
                        Style::default().fg(Color::Gray),
                ),
                Span::styled(
                    format!("Free:{:>4.0}G", disk.avail_gb),
                        Style::default().fg(Color::LightGreen),
                ),
            ]));

            let r_col = if disk.read_mb_s > 0.05 {
                Color::Cyan
            } else {
                Color::DarkGray
            };
            let w_col = if disk.write_mb_s > 0.05 {
                Color::Magenta
            } else {
                Color::DarkGray
            };

            lines.push(Line::from(vec![
                Span::styled(format!("{:>2}% ", pct), Style::default().fg(Color::Green)),
                                  Span::styled(u_f, Style::default().fg(Color::Green)),
                                  Span::styled(u_e, Style::default().fg(Color::DarkGray)),
                                  Span::raw("  "),
                                  Span::styled(
                                      format!("R:{:>5.1}M/s ", disk.read_mb_s),
                                          Style::default().fg(r_col),
                                  ),
                                  Span::styled(
                                      format!("W:{:>5.1}M/s", disk.write_mb_s),
                                          Style::default().fg(w_col),
                                  ),
            ]));
        }

        f.render_widget(Paragraph::new(lines), inner);

        if app.disk_entries.len() > visible_items {
            let mut sb_state =
            ScrollbarState::new(app.disk_entries.len()).position(app.disk_scroll);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                                     inner,
                                     &mut sb_state,
            );
        }
}

fn draw_net_box(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
    .title(Span::styled(
        " net ",
        Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD),
    ))
    .borders(Borders::ALL)
    .border_set(symbols::border::ROUNDED)
    .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Added Constraint::Length(3) to create a clean spacing column between graphs and text
    let split = Layout::default()
    .direction(Direction::Horizontal)
    .constraints([
        Constraint::Percentage(55),
                 Constraint::Length(3),
                 Constraint::Percentage(42),
    ])
    .split(inner);

    let net_graphs = Layout::default()
    .direction(Direction::Vertical)
    .constraints([
        Constraint::Percentage(48),
                 Constraint::Length(1),
                 Constraint::Percentage(48),
    ])
    .split(split[0]); // Rendered in the first split section

    let dl_graph = BlockGraph::new(&app.net_rx_history, app.peak_rx_kib, false, (26, 188, 156))
    .label("▼ DL");
    f.render_widget(dl_graph, net_graphs[0]);

    let mid_div = "─".repeat(net_graphs[1].width as usize);
    f.render_widget(
        Paragraph::new(Span::styled(mid_div, Style::default().fg(Color::DarkGray))),
                    net_graphs[1],
    );

    let ul_graph = BlockGraph::new(&app.net_tx_history, app.peak_tx_kib, true, (155, 89, 182))
    .label("▲ UL");
    f.render_widget(ul_graph, net_graphs[2]);

    let (total_rx, total_tx) = app
    .networks
    .iter()
    .fold((0u64, 0u64), |(tr, tt), (_, d)| {
        (tr + d.total_received(), tt + d.total_transmitted())
    });

    let cur_rx_kib = app.net_rx_history.back().copied().unwrap_or(0);
    let cur_tx_kib = app.net_tx_history.back().copied().unwrap_or(0);

    let dl_style = Style::default().fg(Color::Cyan);
    let ul_style = Style::default().fg(Color::Magenta);

    let info = vec![
        Line::from(Span::styled(
            "download (▼)",
                                dl_style.add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(" Speed: {:>6} KiB/s", cur_rx_kib),
                dl_style,
        )),
        Line::from(Span::styled(
            format!(" Peak:  {:>6} KiB/s", app.peak_rx_kib),
                dl_style,
        )),
        Line::from(Span::styled(
            format!(" Total: {:>6.2} GiB", total_rx as f64 / 1_073_741_824.0),
                dl_style,
        )),
        Line::from(""),
        Line::from(Span::styled(
            "upload (▲)",
                                ul_style.add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(" Speed: {:>6} KiB/s", cur_tx_kib),
                ul_style,
        )),
        Line::from(Span::styled(
            format!(" Peak:  {:>6} KiB/s", app.peak_tx_kib),
                ul_style,
        )),
        Line::from(Span::styled(
            format!(" Total: {:>6.2} GiB", total_tx as f64 / 1_073_741_824.0),
                ul_style,
        )),
    ];

    // Rendered into split[2], skipping split[1] (the empty spacing column)
    f.render_widget(Paragraph::new(info), split[2]);
}

fn draw_proc_box(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.focus == FocusPanel::Processes;
    let border_col = if is_focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };

    let total_procs = app.sys.processes().len();
    let selected_idx = app.proc_state.selected().unwrap_or(0);
    let num_cpus = app.sys.cpus().len().max(1) as f32;

    let block = Block::default()
    .title(Line::from(vec![
        Span::styled(
            " proc ",
            Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("── {}/{} ", selected_idx + 1, total_procs),
                Style::default().fg(Color::Gray),
        ),
    ]))
    .borders(Borders::ALL)
    .border_set(symbols::border::ROUNDED)
    .border_style(Style::default().fg(border_col));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut procs: Vec<_> = app.sys.processes().values().collect();
    procs.sort_by(|a, b| {
        b.cpu_usage()
        .partial_cmp(&a.cpu_usage())
        .unwrap_or(std::cmp::Ordering::Equal)
    });

    let header = Row::new(vec!["Pid:", "Program:", "Command:", "MemB", "Cpu%", "Gpu%"])
    .style(
        Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = procs
    .into_iter()
    .map(|p| {
        let pid_u32 = p.pid().as_u32();
        let pid = pid_u32.to_string();
        let name = p.name().to_string_lossy().to_string();
        let cmd = p
        .cmd()
        .iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
        let cmd_display = if cmd.is_empty() { name.clone() } else { cmd };
        let mem_mb = format!("{}M", p.memory() / (1024 * 1024));
        let cpu = format!("{:>4.1}", p.cpu_usage() / num_cpus);
        let gpu_val = app.gpu.pid_gpu_pct.get(&pid_u32).copied().unwrap_or(0.0);
        let gpu_str = format!("{:>4.1}", gpu_val);
        let gpu_style = if gpu_val > 0.1 {
            Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        Row::new(vec![
            Cell::from(pid),
                 Cell::from(name).style(Style::default().fg(Color::Green)),
                 Cell::from(cmd_display),
                 Cell::from(mem_mb).style(Style::default().fg(Color::Cyan)),
                 Cell::from(cpu).style(Style::default().fg(Color::LightGreen)),
                 Cell::from(gpu_str).style(gpu_style),
        ])
    })
    .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(7),
                           Constraint::Length(13),
                           Constraint::Min(15),
                           Constraint::Length(6),
                           Constraint::Length(6),
                           Constraint::Length(6),
        ],
    )
    .header(header)
    .row_highlight_style(
        Style::default()
        .bg(Color::Rgb(40, 50, 65))
        .add_modifier(Modifier::BOLD),
    )
    .highlight_spacing(HighlightSpacing::Always);

    f.render_stateful_widget(table, inner, &mut app.proc_state);
}

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let tick_rate = Duration::from_millis(500);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| draw_ui(f, &mut app))?;

        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Tab => {
                            app.focus = match app.focus {
                                FocusPanel::Processes => FocusPanel::Disks,
                                FocusPanel::Disks => FocusPanel::Processes,
                            };
                        }
                        KeyCode::Down | KeyCode::Char('j') => app.scroll_down(),
                        KeyCode::Up | KeyCode::Char('k') => app.scroll_up(),
                        KeyCode::PageDown => {
                            for _ in 0..10 {
                                app.scroll_down();
                            }
                        }
                        KeyCode::PageUp => {
                            for _ in 0..10 {
                                app.scroll_up();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.on_tick();
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
