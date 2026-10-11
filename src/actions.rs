//! The only module allowed to change the GPU or request a Windows power-off.
use crate::protection::Actions;
use serde_json::{Value, json};
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

#[derive(Default)]
pub struct Gate {
    pub armed: Arc<AtomicBool>,
    pub epoch: AtomicU64,
    pending: AtomicBool,
    shutdown_lock: Mutex<()>,
    arm_lock: Mutex<()>,
}
impl Gate {
    // Serializes cancellation with a shutdown request. The UI clears armed first,
    // so an in-flight request is either prevented or subsequently aborted.
    pub fn disarm_and_cancel(&self) -> Result<(), String> {
        self.cancel_with(|| shutdown_command(&["/a"]))
    }
    fn cancel_with(&self, cancel: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        self.prevent_actions();
        let _guard = self.shutdown_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.pending.load(Ordering::SeqCst) {
            cancel()?;
            self.pending.store(false, Ordering::SeqCst);
        }
        Ok(())
    }
    pub fn prevent_actions(&self) {
        let _guard = self.arm_lock.lock().unwrap_or_else(|p| p.into_inner());
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.armed.store(false, Ordering::SeqCst);
    }
    pub fn pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }
    pub fn try_arm(&self, epoch: u64) -> bool {
        let _guard = self.arm_lock.lock().unwrap_or_else(|p| p.into_inner());
        if self.epoch.load(Ordering::SeqCst) != epoch
            || self.armed.load(Ordering::SeqCst)
            || self.pending()
        {
            return false;
        }
        self.armed.store(true, Ordering::SeqCst);
        true
    }
    fn request_with(&self, request: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let _guard = self.shutdown_lock.lock().unwrap_or_else(|p| p.into_inner());
        if !self.armed.load(Ordering::SeqCst) {
            return Err("Protection disarmed".into());
        }
        request()?;
        self.pending.store(true, Ordering::SeqCst);
        Ok(())
    }
}
fn system_dir() -> Result<PathBuf, String> {
    let mut buffer = [0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("Cannot locate Windows System32".into());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}
fn shutdown_command(args: &[&str]) -> Result<(), String> {
    let output = Command::new(system_dir()?.join("shutdown.exe"))
        .args(args)
        .creation_flags(0x08000000)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "shutdown.exe rejected request ({}): {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
}
pub fn emergency_cancel() -> Result<(), String> {
    shutdown_command(&["/a"])
}

// A replacement is complete before it overwrites the prior recovery information.
fn save_marker(path: &Path, record: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid marker path")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec(record).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let wide = |p: &Path| {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    if unsafe {
        MoveFileExW(
            wide(&temporary).as_ptr(),
            wide(path).as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

trait Driver {
    fn uuid(&self) -> Result<String, String>;
    fn power(&self) -> Result<u32, String>;
    fn constraints(&self) -> Result<(u32, u32), String>;
    fn floor(&self) -> Option<u32>;
    fn lock(&mut self, mhz: u32) -> Result<(), String>;
    fn set_power(&mut self, mw: u32) -> Result<(), String>;
    fn unlock(&mut self) -> Result<(), String>;
}

struct Mitigation<D: Driver> {
    driver: D,
    marker: PathBuf,
}
impl<D: Driver> Mitigation<D> {
    fn limit(&mut self, armed: &AtomicBool) -> Result<String, String> {
        if self.marker.exists() {
            return Err("Previous GPU limit needs restoration/acknowledgement first".into());
        }
        let saved = self.driver.power()?;
        let (minimum, maximum) = self.driver.constraints()?;
        if minimum == 0 || minimum > maximum || !(minimum..=maximum).contains(&saved) {
            return Err("Invalid NVML power limits".into());
        }
        let uuid = self.driver.uuid()?;
        // Persist intent BEFORE each write: recovery works even if we crash between
        // the driver accepting a command and recording its result.
        let mut record = json!({"schema_version":1,"gpu_uuid":uuid,"saved_power_limit_mw":saved,
            "restore_clocks":false,"restore_power":false});
        let mut clocks = None;
        let mut errors = Vec::new();
        let mut candidates = Vec::new();
        if let Some(floor) = self.driver.floor() {
            candidates.push(floor);
        }
        for fallback in [500, 700, 1000] {
            if !candidates.contains(&fallback) {
                candidates.push(fallback);
            }
        }
        for mhz in candidates {
            if !armed.load(Ordering::SeqCst) {
                break;
            }
            record["restore_clocks"] = true.into();
            save_marker(&self.marker, &record)?;
            match self.driver.lock(mhz) {
                Ok(()) => {
                    clocks = Some(mhz);
                    break;
                }
                Err(e) => errors.push(e),
            }
        }
        let mut power = false;
        if armed.load(Ordering::SeqCst) {
            record["restore_power"] = true.into();
            save_marker(&self.marker, &record)?;
            match self.driver.set_power(minimum) {
                Ok(()) => power = true,
                Err(e) => errors.push(e),
            }
        }
        // Retain attempted changes on disk, including ambiguous driver failures.
        if clocks.is_none() && !power {
            return Err(format!(
                "Neither GPU limit succeeded: {}",
                errors.join("; ")
            ));
        }
        Ok(format!(
            "clock lock: {}; power limit: {}; {}",
            clocks
                .map(|c| format!("{c} MHz"))
                .unwrap_or_else(|| "unavailable".into()),
            if power {
                format!("{} W", minimum / 1000)
            } else {
                "unchanged".into()
            },
            errors.join("; ")
        ))
    }
    fn restore(&mut self, armed: Option<&AtomicBool>) -> Result<(), String> {
        if !self.marker.exists() {
            return Ok(());
        }
        let file = File::open(&self.marker).map_err(|e| e.to_string())?;
        if file.metadata().map_err(|e| e.to_string())?.len() > 4096 {
            return Err("Invalid recovery marker size".into());
        }
        let mut record: Value = serde_json::from_reader(file).map_err(|e| e.to_string())?;
        if record["schema_version"] != 1
            || record["gpu_uuid"].as_str() != Some(self.driver.uuid()?.as_str())
        {
            return Err("Recovery marker belongs to a different GPU or schema".into());
        }
        let saved = record["saved_power_limit_mw"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or("Invalid saved power limit")?;
        let (min, max) = self.driver.constraints()?;
        if !(min..=max).contains(&saved) {
            return Err("Saved power limit outside driver constraints".into());
        }
        let restore_power = record["restore_power"]
            .as_bool()
            .ok_or("Invalid recovery power flag")?;
        let restore_clocks = record["restore_clocks"]
            .as_bool()
            .ok_or("Invalid recovery clock flag")?;
        let mut errors = Vec::new();
        let permitted = || {
            if armed.is_some_and(|a| !a.load(Ordering::SeqCst)) {
                Err("Protection disarmed during automatic restoration".to_string())
            } else {
                Ok(())
            }
        };
        if restore_power {
            permitted()?;
            match self.driver.set_power(saved) {
                Ok(()) => {
                    record["restore_power"] = false.into();
                    save_marker(&self.marker, &record)?;
                }
                Err(e) => errors.push(e),
            }
        }
        if restore_clocks {
            permitted()?;
            match self.driver.unlock() {
                Ok(()) => {
                    record["restore_clocks"] = false.into();
                    save_marker(&self.marker, &record)?;
                }
                Err(e) => errors.push(e),
            }
        }
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        std::fs::remove_file(&self.marker).map_err(|e| e.to_string())
    }
}

pub struct RealActions {
    gate: Arc<Gate>,
    mitigation: Result<Mitigation<Nvml>, String>,
    folder: PathBuf,
}
impl RealActions {
    pub fn new(subsystem: u32, folder: PathBuf, gate: Arc<Gate>) -> Self {
        let mitigation = Nvml::new(subsystem).map(|driver| Mitigation {
            driver,
            marker: folder.join("mitigation.json"),
        });
        Self {
            gate,
            mitigation,
            folder,
        }
    }
    pub fn readiness(&self) -> String {
        match &self.mitigation {
            Ok(m) => match m.driver.constraints() {
                Ok((min, max)) => format!(
                    "NVML connected; power range {}–{} W (writes require administrator)",
                    min / 1000,
                    max / 1000
                ),
                Err(e) => e,
            },
            Err(e) => format!("GPU limiting unavailable; threshold triggers will power off: {e}"),
        }
    }
}
impl Actions for RealActions {
    fn limit(&mut self) -> Result<String, String> {
        if !self.gate.armed.load(Ordering::SeqCst) {
            return Err("Protection disarmed".into());
        }
        self.mitigation
            .as_mut()
            .map_err(|e| e.clone())?
            .limit(&self.gate.armed)
    }
    fn restore(&mut self) -> Result<(), String> {
        self.mitigation
            .as_mut()
            .map_err(|e| e.clone())?
            .restore(Some(&self.gate.armed))
    }
    fn restore_manual(&mut self) -> Result<(), String> {
        self.mitigation
            .as_mut()
            .map_err(|e| e.clone())?
            .restore(None)
    }
    fn shutdown(&mut self, reason: &str) -> Result<(), String> {
        let marker = json!({"schema_version":1,"reason":reason,"unix_time_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64});
        // Record a reason separately from rotating history; logging failure must
        // never prevent an already-armed protective power-off.
        self.gate.request_with(|| {
            let _ = save_marker(&self.folder.join("last_shutdown.json"), &marker);
            shutdown_command(&["/s", "/t", "5", "/f", "/c", reason])
        })
    }
}

#[repr(C)]
#[derive(Default)]
struct Pci {
    legacy: [u8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    device_id: u32,
    subsystem: u32,
    bus_id: [u8; 32],
}
type Device = *mut c_void;
type Init = unsafe extern "C" fn() -> i32;
type GetCount = unsafe extern "C" fn(*mut u32) -> i32;
type GetHandle = unsafe extern "C" fn(u32, *mut Device) -> i32;
type GetPci = unsafe extern "C" fn(Device, *mut Pci) -> i32;
type GetUuid = unsafe extern "C" fn(Device, *mut u8, u32) -> i32;
type GetPower = unsafe extern "C" fn(Device, *mut u32) -> i32;
type Constraints = unsafe extern "C" fn(Device, *mut u32, *mut u32) -> i32;
type SetPower = unsafe extern "C" fn(Device, u32) -> i32;
type Lock = unsafe extern "C" fn(Device, u32, u32) -> i32;
type Unlock = unsafe extern "C" fn(Device) -> i32;
type MemClocks = unsafe extern "C" fn(Device, *mut u32, *mut u32) -> i32;
type GpuClocks = unsafe extern "C" fn(Device, u32, *mut u32, *mut u32) -> i32;
struct Nvml {
    dll: HMODULE,
    initialized: bool,
    shutdown: Init,
    device: Device,
    get_uuid: GetUuid,
    get_power: GetPower,
    constraints: Constraints,
    set_power: SetPower,
    lock: Option<Lock>,
    unlock: Option<Unlock>,
    mem_clocks: Option<MemClocks>,
    gpu_clocks: Option<GpuClocks>,
}
fn check(status: i32, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: NVML status {status}"))
    }
}
impl Nvml {
    fn new(subsystem: u32) -> Result<Self, String> {
        let name: Vec<u16> = "nvml.dll".encode_utf16().chain(Some(0)).collect();
        let dll = unsafe {
            LoadLibraryExW(
                name.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if dll.is_null() {
            return Err(format!(
                "System32 NVML unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Keep an RAII library guard even if symbol lookup fails before init.
        struct Library(HMODULE);
        impl Drop for Library {
            fn drop(&mut self) {
                unsafe {
                    FreeLibrary(self.0);
                }
            }
        }
        let library = Library(dll);
        macro_rules! optional {
            ($name:literal,$ty:ty) => {{
                let symbol = unsafe { GetProcAddress(dll, concat!($name, "\0").as_ptr()) };
                symbol.map(|f| unsafe {
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(f)
                })
            }};
        }
        macro_rules! required {
            ($name:literal,$ty:ty) => {
                optional!($name, $ty).ok_or(concat!("Missing NVML function: ", $name))?
            };
        }
        let init = required!("nvmlInit_v2", Init);
        let count = required!("nvmlDeviceGetCount_v2", GetCount);
        let handle = required!("nvmlDeviceGetHandleByIndex_v2", GetHandle);
        let pci = required!("nvmlDeviceGetPciInfo_v3", GetPci);
        let mut api = Self {
            dll,
            initialized: false,
            shutdown: required!("nvmlShutdown", Init),
            device: std::ptr::null_mut(),
            get_uuid: required!("nvmlDeviceGetUUID", GetUuid),
            get_power: required!("nvmlDeviceGetPowerManagementLimit", GetPower),
            constraints: required!("nvmlDeviceGetPowerManagementLimitConstraints", Constraints),
            set_power: required!("nvmlDeviceSetPowerManagementLimit", SetPower),
            lock: optional!("nvmlDeviceSetGpuLockedClocks", Lock),
            unlock: optional!("nvmlDeviceResetGpuLockedClocks", Unlock),
            mem_clocks: optional!("nvmlDeviceGetSupportedMemoryClocks", MemClocks),
            gpu_clocks: optional!("nvmlDeviceGetSupportedGraphicsClocks", GpuClocks),
        };
        std::mem::forget(library);
        check(unsafe { init() }, "initialize")?;
        api.initialized = true;
        let mut n = 0;
        check(unsafe { count(&mut n) }, "GPU count")?;
        if n > 64 {
            return Err("Invalid GPU count".into());
        }
        let mut matches = Vec::new();
        for index in 0..n {
            let mut dev = std::ptr::null_mut();
            check(unsafe { handle(index, &mut dev) }, "GPU handle")?;
            let mut info = Pci::default();
            check(unsafe { pci(dev, &mut info) }, "GPU PCI identity")?;
            if info.subsystem == subsystem {
                matches.push(dev);
            }
        }
        if matches.len() != 1 {
            return Err(
                "NVML GPU identity is missing or ambiguous; refusing to control another GPU".into(),
            );
        }
        api.device = matches[0];
        Ok(api)
    }
}
impl Driver for Nvml {
    fn uuid(&self) -> Result<String, String> {
        let mut buf = [0u8; 96];
        check(
            unsafe { (self.get_uuid)(self.device, buf.as_mut_ptr(), buf.len() as u32) },
            "UUID",
        )?;
        let end = buf.iter().position(|b| *b == 0).ok_or("Invalid GPU UUID")?;
        String::from_utf8(buf[..end].to_vec()).map_err(|e| e.to_string())
    }
    fn power(&self) -> Result<u32, String> {
        let mut value = 0;
        check(
            unsafe { (self.get_power)(self.device, &mut value) },
            "read power limit",
        )?;
        Ok(value)
    }
    fn constraints(&self) -> Result<(u32, u32), String> {
        let (mut min, mut max) = (0, 0);
        check(
            unsafe { (self.constraints)(self.device, &mut min, &mut max) },
            "power constraints",
        )?;
        Ok((min, max))
    }
    fn lock(&mut self, mhz: u32) -> Result<(), String> {
        let f = self.lock.ok_or("Clock locking unavailable")?;
        if self.unlock.is_none() {
            return Err("Clock reset unavailable; refusing unrestorable lock".into());
        }
        check(unsafe { f(self.device, mhz, mhz) }, "lock GPU clocks")
    }
    fn set_power(&mut self, mw: u32) -> Result<(), String> {
        check(
            unsafe { (self.set_power)(self.device, mw) },
            "set power limit",
        )
    }
    fn unlock(&mut self) -> Result<(), String> {
        let f = self.unlock.ok_or("Clock reset unavailable")?;
        check(unsafe { f(self.device) }, "reset GPU clocks")
    }
    fn floor(&self) -> Option<u32> {
        let (mem, gpu) = (self.mem_clocks?, self.gpu_clocks?);
        let enumerate = |f: &dyn Fn(*mut u32, *mut u32) -> i32| -> Option<Vec<u32>> {
            let mut n = 0;
            let status = f(&mut n, std::ptr::null_mut());
            if (status != 0 && status != 7) || n == 0 || n > 256 {
                return None;
            }
            let mut data = vec![0; n as usize];
            let capacity = n;
            if f(&mut n, data.as_mut_ptr()) != 0 || n > capacity {
                return None;
            }
            data.truncate(n as usize);
            Some(data)
        };
        let memory = enumerate(&|n, p| unsafe { mem(self.device, n, p) })?;
        let mut minimum = None;
        for mhz in [*memory.first()?, *memory.last()?] {
            if let Some(clocks) = enumerate(&|n, p| unsafe { gpu(self.device, mhz, n, p) }) {
                for clock in clocks {
                    if clock > 0 {
                        minimum = Some(minimum.map_or(clock, |previous: u32| previous.min(clock)));
                    }
                }
            }
        }
        minimum
    }
}
impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            if self.initialized {
                (self.shutdown)();
            }
            FreeLibrary(self.dll);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        writes: Vec<String>,
        fail_power: bool,
        fail_clock: bool,
        fail_restore: bool,
    }
    impl Driver for Fake {
        fn uuid(&self) -> Result<String, String> {
            Ok("GPU-test".into())
        }
        fn power(&self) -> Result<u32, String> {
            Ok(450000)
        }
        fn constraints(&self) -> Result<(u32, u32), String> {
            Ok((400000, 600000))
        }
        fn floor(&self) -> Option<u32> {
            Some(210)
        }
        fn lock(&mut self, mhz: u32) -> Result<(), String> {
            self.writes.push(format!("clock {mhz}"));
            if self.fail_clock {
                Err("no clock".into())
            } else {
                Ok(())
            }
        }
        fn set_power(&mut self, mw: u32) -> Result<(), String> {
            self.writes.push(format!("power {mw}"));
            if self.fail_power || (self.fail_restore && mw == 450000) {
                Err("no power".into())
            } else {
                Ok(())
            }
        }
        fn unlock(&mut self) -> Result<(), String> {
            self.writes.push("unlock".into());
            Ok(())
        }
    }
    fn fixture(name: &str) -> Mitigation<Fake> {
        let folder =
            std::env::temp_dir().join(format!("astral-control-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        Mitigation {
            driver: Fake::default(),
            marker: folder.join("mitigation.json"),
        }
    }
    fn cleanup(m: Mitigation<Fake>) {
        if m.marker.exists() {
            std::fs::remove_file(&m.marker).unwrap();
        }
        std::fs::remove_dir(m.marker.parent().unwrap()).unwrap();
    }
    #[test]
    fn limits_then_restores_original_custom_power_cap() {
        let mut m = fixture("restore");
        m.limit(&AtomicBool::new(true)).unwrap();
        assert_eq!(m.driver.writes, ["clock 210", "power 400000"]);
        assert!(m.marker.exists());
        m.restore(None).unwrap();
        assert_eq!(
            m.driver.writes,
            ["clock 210", "power 400000", "power 450000", "unlock"]
        );
        assert!(!m.marker.exists());
        cleanup(m);
    }
    #[test]
    fn failed_restoration_preserves_marker_and_retries_only_remaining_changes() {
        let mut m = fixture("retry");
        m.limit(&AtomicBool::new(true)).unwrap();
        m.driver.fail_restore = true;
        assert!(m.restore(None).is_err());
        assert!(m.marker.exists());
        m.driver.fail_restore = false;
        m.restore(None).unwrap();
        assert_eq!(
            m.driver
                .writes
                .iter()
                .filter(|s| s.as_str() == "unlock")
                .count(),
            1
        );
        cleanup(m);
    }
    #[test]
    fn clock_ladder_and_partial_mitigation() {
        let mut m = fixture("partial");
        m.driver.fail_clock = true;
        m.limit(&AtomicBool::new(true)).unwrap();
        assert_eq!(
            m.driver.writes,
            [
                "clock 210",
                "clock 500",
                "clock 700",
                "clock 1000",
                "power 400000"
            ]
        );
        m.driver.fail_clock = false;
        m.restore(None).unwrap();
        cleanup(m);
    }
    #[test]
    fn disarmed_gate_prevents_all_driver_writes() {
        let mut m = fixture("disarmed");
        assert!(m.limit(&AtomicBool::new(false)).is_err());
        assert!(m.driver.writes.is_empty());
        cleanup(m);
    }
    #[test]
    fn recovery_rejects_wrong_gpu_and_corrupt_power_cap() {
        let mut m = fixture("identity");
        save_marker(&m.marker,&json!({"schema_version":1,"gpu_uuid":"GPU-other","saved_power_limit_mw":450000,"restore_clocks":true,"restore_power":true})).unwrap();
        assert!(m.restore(None).is_err());
        assert!(m.driver.writes.is_empty());
        save_marker(&m.marker,&json!({"schema_version":1,"gpu_uuid":"GPU-test","saved_power_limit_mw":999999,"restore_clocks":true,"restore_power":true})).unwrap();
        assert!(m.restore(None).is_err());
        assert!(m.driver.writes.is_empty());
        cleanup(m);
    }
    #[test]
    fn marker_failure_prevents_driver_changes() {
        let mut m = fixture("marker");
        let blocker = m.marker.parent().unwrap().join("blocker");
        std::fs::write(&blocker, b"file").unwrap();
        let original = m.marker.clone();
        m.marker = blocker.join("mitigation.json");
        assert!(m.limit(&AtomicBool::new(true)).is_err());
        assert!(m.driver.writes.is_empty());
        std::fs::remove_file(blocker).unwrap();
        m.marker = original;
        cleanup(m);
    }
    #[test]
    fn pci_abi_layout() {
        assert_eq!(std::mem::size_of::<Pci>(), 68);
    }
    #[test]
    fn gate_starts_disarmed_and_cancel_without_pending_does_not_call_windows() {
        let gate = Gate::default();
        assert!(!gate.armed.load(Ordering::SeqCst));
        assert!(!gate.pending());
        gate.disarm_and_cancel().unwrap();
    }
    #[test]
    fn old_arm_requests_cannot_override_disarm() {
        let gate = Gate::default();
        let epoch = gate.epoch.load(Ordering::SeqCst);
        gate.prevent_actions();
        assert!(!gate.try_arm(epoch));
        assert!(!gate.armed.load(Ordering::SeqCst));
        assert!(gate.try_arm(gate.epoch.load(Ordering::SeqCst)));
        assert!(!gate.try_arm(gate.epoch.load(Ordering::SeqCst)));
    }
    #[test]
    fn disarm_prevents_shutdown_and_failed_requests_do_not_latch() {
        let gate = Gate::default();
        assert!(
            gate.request_with(|| panic!("disarmed must not call Windows"))
                .is_err()
        );
        assert!(gate.try_arm(0));
        assert!(gate.request_with(|| Err("access denied".into())).is_err());
        assert!(!gate.pending());
        gate.request_with(|| Ok(())).unwrap();
        assert!(gate.pending());
        assert!(gate.cancel_with(|| Err("cancel denied".into())).is_err());
        assert!(gate.pending());
        assert!(!gate.armed.load(Ordering::SeqCst));
        gate.cancel_with(|| Ok(())).unwrap();
        assert!(!gate.pending());
    }
    #[test]
    fn cancellation_waits_for_an_in_flight_request_then_aborts_it() {
        use std::sync::mpsc;
        use std::time::Duration;
        let gate = Arc::new(Gate::default());
        assert!(gate.try_arm(0));
        let (started_rx_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let requester = gate.clone();
        let request = std::thread::spawn(move || {
            requester.request_with(|| {
                started_rx_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(())
            })
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        gate.prevent_actions();
        let canceller = gate.clone();
        let cancel = std::thread::spawn(move || canceller.cancel_with(|| Ok(())));
        release_tx.send(()).unwrap();
        request.join().unwrap().unwrap();
        cancel.join().unwrap().unwrap();
        assert!(!gate.pending());
        assert!(!gate.armed.load(Ordering::SeqCst));
    }
    #[test]
    fn automatic_restore_is_blocked_after_disarming_but_manual_restore_works() {
        let mut m = fixture("restore-disarmed");
        let armed = AtomicBool::new(true);
        m.limit(&armed).unwrap();
        let before = m.driver.writes.clone();
        armed.store(false, Ordering::SeqCst);
        assert!(m.restore(Some(&armed)).is_err());
        assert_eq!(m.driver.writes, before);
        assert!(m.marker.exists());
        m.restore(None).unwrap();
        assert!(!m.marker.exists());
        cleanup(m);
    }
}
