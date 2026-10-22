//! iPhone Duo, a native device type since Xcode 27.1: one simulator with a cover
//! display (`primary`) and an inner display (`primary-1`). `devicectl` reports which
//! display is active; a small helper run inside the simulator (vendored from duoctl,
//! `duo_helper.m`) moves the hinge and touches the inner display, which AXe cannot reach.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, PoisonError,
    },
    time::{Duration, Instant},
};

use super::{json, parse_devices, valid_udid, xcrun, Size, QUICK};
use crate::workspace::{checked, run_process};

const MIN_RUNTIME: [u64; 2] = [27, 1];
const NAME: &str = "iPhone Duo";
pub const COVER: &str = "primary";
pub const INNER: &str = "primary-1";
const UNAVAILABLE: &str = "iPhone Duo needs Xcode 27.1 or later with the iOS 27.1 simulator runtime (Xcode → Settings → Components). Xcode 27.1 requires macOS 26.6.";
const HELPER: &str = include_str!("duo_helper.m");
/// The hinge sweeps briefly: SpringBoard missed closes that jumped or swept faster
/// (Xcode 27.1, 10 open/close cycles each: jump 5/10, 0.1 s 0/10, 0.2 s 10/10 in 0.8 s,
/// 0.8 s 10/10 in 1.65 s). The window animates the fold itself meanwhile.
const SWEEP_SECONDS: &str = "0.2";
/// A close sent right after the last switch was lost (0/10 at once, 16/16 after 0.3 s).
const SETTLE: Duration = Duration::from_millis(500);
/// A lost move is retried after this: switches take about 0.8 s.
const RETRY_AFTER: Duration = Duration::from_secs(2);
/// How long the matching display may take to light up, polled at `SWITCH_POLL`.
const SWITCH: Duration = Duration::from_secs(6);
const SWITCH_POLL: Duration = Duration::from_millis(50);
/// When each Duo last switched displays.
// ponytail: grows by one entry per Duo ever folded; fine for the one or two that exist.
static SWITCHED: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
/// Held for a whole fold; reading elements waits for it, so its accessibility hit tests
/// never run while the hinge moves and the displays switch.
static HINGE: Mutex<()> = Mutex::new(());
/// A lost fold is recovered from here: when a sweep was lost, sweeping again or back did
/// not move the hinge (Xcode 27.1), but stepping to 90° and then to the pose did.
const MIDWAY: u32 = 90;

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Pose {
    #[default]
    Closed,
    Book,
    Open,
}
impl Pose {
    pub fn degrees(self) -> u32 {
        match self { Pose::Closed => 0, Pose::Book => 127, Pose::Open => 180 }
    }
}
/// Closed shows the cover; Book and Open run the app on the inner display.
pub fn display_for(pose: Pose) -> &'static str {
    if pose == Pose::Closed { COVER } else { INNER }
}
/// `devicectl` cannot tell Book from Open, so the recorded pose stands unless the
/// active display contradicts it.
pub fn reconcile(recorded: Pose, inner_active: Option<bool>) -> Pose {
    match (recorded, inner_active) {
        (_, Some(false)) => Pose::Closed,
        (Pose::Closed, Some(true)) => Pose::Open,
        (pose, _) => pose,
    }
}

/// One display as `devicectl device info displays` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Display {
    pub inner: bool,
    pub active: bool,
    pub uuid: String,
    /// The display number accessibility hit-tests take.
    pub id: u32,
    /// Clockwise quarter turns of the interface: 0, 90, 180 or 270.
    pub rotation: u16,
    /// Points in the panel's native (unrotated) axes.
    pub native: (f64, f64),
}
impl Display {
    /// Points as the interface sees it now.
    pub fn ui(&self) -> (f64, f64) {
        if self.rotation % 180 == 90 { (self.native.1, self.native.0) } else { self.native }
    }
    pub fn name(&self) -> &'static str {
        if self.inner { "inner" } else { "cover" }
    }
}
pub fn parse_displays(value: &Value) -> Vec<Display> {
    value["result"]["displays"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["name"].as_str().is_some_and(|n| n.starts_with("LCD")))
        .filter_map(|d| {
            let scale = d["pointScale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
            let size = &d["bounds"][1];
            Some(Display {
                inner: d["primary"].as_bool() == Some(false) || d["name"] != "LCD",
                active: d["active"].as_bool() == Some(true),
                uuid: d["uniqueId"].as_str()?.to_string(),
                id: d["displayId"].as_u64().and_then(|id| u32::try_from(id).ok())?,
                rotation: d["currentOrientation"].as_str().and_then(|r| r.strip_prefix("rot")).and_then(|r| r.parse().ok()).filter(|r| [0, 90, 180, 270].contains(r)).unwrap_or(0),
                native: ((size[0].as_f64()? / scale).round(), (size[1].as_f64()? / scale).round()),
            })
        })
        .collect()
}
pub fn active(displays: &[Display]) -> Option<&Display> {
    displays.iter().find(|d| d.active)
}
pub fn displays(udid: &str) -> Result<Vec<Display>, String> {
    let list = parse_displays(&json(&xcrun(&["devicectl", "device", "info", "displays", "--device", udid, "-j", "-"], QUICK)?)?);
    if list.is_empty() { Err("iPhone Duo reported no displays. Is it still booting?".into()) } else { Ok(list) }
}

/// Interface points (`u`, `v` normalized) to the panel's normalized native axes.
pub fn to_native(rotation: u16, u: f64, v: f64) -> (f64, f64) {
    match rotation {
        90 => (v, 1.0 - u),
        180 => (1.0 - u, 1.0 - v),
        270 => (1.0 - v, u),
        _ => (u, v),
    }
}
/// A rectangle in native points (`native` is the panel's size) to interface points.
pub fn rect_to_ui(rotation: u16, native: (f64, f64), [x, y, w, h]: [f64; 4]) -> [f64; 4] {
    let (nw, nh) = native;
    match rotation {
        90 => [nh - (y + h), x, h, w],
        180 => [nw - (x + w), nh - (y + h), w, h],
        270 => [y, nw - (x + w), h, w],
        _ => [x, y, w, h],
    }
}

pub fn is_duo_type(identifier: &str) -> bool {
    identifier.contains("iPhone-Duo") || identifier.contains(".V68")
}
pub fn pick_type(types: &Value) -> Option<String> {
    types["devicetypes"].as_array()?.iter().filter_map(|t| t["identifier"].as_str()).find(|id| is_duo_type(id)).map(str::to_string)
}
/// Identifier of the newest available iOS runtime that has the Duo (27.1 or later).
pub fn pick_runtime(runtimes: &Value) -> Option<String> {
    let version = |r: &Value| -> Vec<u64> { r["version"].as_str().unwrap_or_default().split('.').map(|n| n.parse().unwrap_or(0)).collect() };
    runtimes["runtimes"]
        .as_array()?
        .iter()
        .filter(|r| r["isAvailable"].as_bool() == Some(true) && r["platform"].as_str() == Some("iOS") && version(r)[..] >= MIN_RUNTIME[..])
        .max_by_key(|r| version(r))
        .and_then(|r| r["identifier"].as_str())
        .map(str::to_string)
}
pub fn available(types: &Value, runtimes: &Value) -> bool {
    pick_type(types).is_some() && pick_runtime(runtimes).is_some()
}
pub fn probe() -> bool {
    let list = |what: &str| xcrun(&["simctl", "list", what, "-j"], QUICK).and_then(|o| json(&o));
    matches!((list("devicetypes"), list("runtimes")), (Ok(t), Ok(r)) if available(&t, &r))
}

/// The Duo's UDID: an existing one, or a new simulator on the native type.
pub fn create() -> Result<String, String> {
    let existing = xcrun(&["simctl", "list", "devices", "available", "-j"], QUICK).and_then(|o| json(&o))?;
    if let Some(duo) = parse_devices(&existing).into_iter().find(|d| d.duo) {
        return Ok(duo.udid);
    }
    let list = |what: &str| xcrun(&["simctl", "list", what, "-j"], QUICK).and_then(|o| json(&o));
    let (device_type, runtime) = pick_type(&list("devicetypes")?).zip(pick_runtime(&list("runtimes")?)).ok_or(UNAVAILABLE)?;
    let output = xcrun(&["simctl", "create", NAME, &device_type, &runtime], Duration::from_secs(60))?;
    let udid = String::from_utf8_lossy(&output.bytes).trim().to_string();
    if valid_udid(&udid) { Ok(udid) } else { Err("The simulator tools returned an unexpected response.".into()) }
}

/// A display from `capabilities.displays` (`primary`, `primary-1`) in points as the
/// user holds it, and its scale. The Duo's profile has no main screen size.
pub fn parse_display(caps: &Value, name: &str) -> Option<(Size, f64)> {
    let d = caps["capabilities"]["displays"].as_array()?.iter().find(|d| d["deviceName"] == name)?;
    let scale = d["scale"].as_f64().filter(|s| *s > 0.0)?;
    let (w, h) = (d["width"].as_f64()? / scale, d["height"].as_f64()? / scale);
    let turned = d["nativeOrientation"].as_f64().is_some_and(|o| o == 90.0 || o == 270.0);
    let (width, height) = if turned { (h, w) } else { (w, h) };
    Some((Size { width: width.round(), height: height.round() }, scale))
}
pub fn parse_inner(caps: &Value) -> Option<Size> {
    parse_display(caps, INNER).map(|(size, _)| size)
}

fn build_args(source: &Path, out: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["-sdk", "iphonesimulator", "clang", "-fobjc-arc", "-O2", "-arch", "arm64", "-mios-simulator-version-min=17.0", "-framework", "Foundation", "-framework", "IOKit"].map(str::to_string).into();
    args.extend([source.to_string_lossy().into_owned(), "-o".into(), out.to_string_lossy().into_owned()]);
    args
}
fn tool(program: &str, args: &[String], label: &str) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    Ok(String::from_utf8_lossy(&checked(run_process(command, label, 1024 * 1024, Duration::from_secs(120))?)?.bytes).trim().to_string())
}
/// The helper for the selected Xcode, compiled and ad-hoc signed on first use, and
/// remembered so taps and folds skip `xcode-select` while the binary is there.
fn helper_path() -> Result<PathBuf, String> {
    static KNOWN: Mutex<Option<PathBuf>> = Mutex::new(None);
    let mut known = KNOWN.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(path) = known.as_ref().filter(|p| p.is_file()) {
        return Ok(path.clone());
    }
    let path = build_helper()?;
    *known = Some(path.clone());
    Ok(path)
}
fn build_helper() -> Result<PathBuf, String> {
    let developer = tool("/usr/bin/xcode-select", &["-p".into()], "xcode-select")?;
    let key: String = Sha256::digest([HELPER.as_bytes(), developer.as_bytes()].concat()).iter().take(6).map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join("indocode-simulator");
    let binary = dir.join(format!("duo-helper-{key}"));
    if binary.is_file() {
        return Ok(binary);
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stem = dir.join(format!("duo-helper-{key}-{}", crate::storage::id()));
    let (source, built) = (stem.with_extension("m"), stem.with_extension("tmp"));
    let result = std::fs::write(&source, HELPER)
        .map_err(|e| e.to_string())
        .and_then(|_| tool("/usr/bin/xcrun", &build_args(&source, &built), "clang"))
        .and_then(|_| tool("/usr/bin/codesign", &["-f".into(), "-s".into(), "-".into(), built.to_string_lossy().into_owned()], "codesign"))
        .and_then(|_| std::fs::rename(&built, &binary).map_err(|e| e.to_string()));
    let _ = std::fs::remove_file(&source);
    let _ = std::fs::remove_file(&built);
    result.map(|_| binary).map_err(|e| format!("Could not build the iPhone Duo helper: {e}"))
}
/// Run the helper inside the simulator; returns what it printed.
pub fn helper(udid: &str, args: &[String]) -> Result<String, String> {
    if !valid_udid(udid) {
        return Err("Unknown simulator.".into());
    }
    let path = helper_path()?.to_string_lossy().into_owned();
    let mut all = vec!["simctl", "spawn", udid, path.as_str()];
    all.extend(args.iter().map(String::as_str));
    xcrun(&all, QUICK).map(|o| String::from_utf8_lossy(&o.bytes).into_owned())
}

/// Labeled elements on `display` with frames in interface points, read inside the
/// simulator: AXe frames SpringBoard's icon list on the inner display in the list's own
/// space (the inner Settings icon came back at the cover's 22.7, 52) and misses most of
/// an app's elements there.
pub fn elements(udid: &str, display: &Display) -> Result<Vec<(String, [f64; 4])>, String> {
    let _hinge = HINGE.lock().unwrap_or_else(PoisonError::into_inner);
    let (w, h) = display.native;
    parse_elements(&helper(udid, &["elements".into(), display.id.to_string(), w.to_string(), h.to_string(), display.rotation.to_string()])?, display)
}
fn parse_elements(output: &str, display: &Display) -> Result<Vec<(String, [f64; 4])>, String> {
    Ok(serde_json::from_str::<Vec<Value>>(output.trim())
        .map_err(|e| format!("The iPhone Duo helper returned unreadable elements: {e}"))?
        .iter()
        .filter_map(|e| {
            let f = e["frame"].as_array()?.iter().map(Value::as_f64).collect::<Option<Vec<_>>>()?;
            let frame = f.try_into().ok()?;
            // SpringBoard answers in native points, apps in interface points.
            Some((e["label"].as_str()?.to_string(), if e["ui"] == true { frame } else { rect_to_ui(display.rotation, display.native, frame) }))
        })
        .collect())
}

/// Runs one request at a time where only the newest matters: a request overtaken by a
/// newer one while it waited is dropped, and running work can ask whether it was overtaken.
#[derive(Default)]
pub struct LatestWins {
    seq: AtomicU64,
    turn: Mutex<()>,
}
impl LatestWins {
    pub fn run<T>(&self, work: impl FnOnce(&dyn Fn() -> bool) -> T) -> Option<T> {
        let ticket = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let _turn = self.turn.lock().unwrap_or_else(PoisonError::into_inner);
        let overtaken = || self.seq.load(Ordering::SeqCst) != ticket;
        if overtaken() {
            return None;
        }
        Some(work(&overtaken))
    }
}

/// Move the hinge from `from` to `to`, then wait until the matching display is active.
/// Returns the displays as last read, so callers need not ask `devicectl` again, or
/// `None` when `overtaken` says a newer fold is waiting, which then does the waiting.
pub fn fold(udid: &str, from: Pose, to: Pose, overtaken: &dyn Fn() -> bool) -> Result<Option<Vec<Display>>, String> {
    let _hinge = HINGE.lock().unwrap_or_else(PoisonError::into_inner);
    let sweep = |a: u32, b: u32| helper(udid, &["hinge-sweep".into(), a.to_string(), b.to_string(), SWEEP_SECONDS.into()]);
    let wanted = to != Pose::Closed;
    let last = SWITCHED.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(u, _)| u == udid).map(|(_, at)| *at);
    std::thread::sleep(last.map_or(Duration::ZERO, |at| SETTLE.saturating_sub(at.elapsed())));
    sweep(from.degrees(), to.degrees())?;
    let switched = match wait(udid, wanted, RETRY_AFTER, overtaken) {
        Some(done) => done,
        None => {
            helper(udid, &["hinge".into(), MIDWAY.to_string()])?;
            std::thread::sleep(SETTLE * 2);
            sweep(MIDWAY, to.degrees())?;
            wait(udid, wanted, SWITCH - RETRY_AFTER, overtaken)
                .ok_or_else(|| format!("iPhone Duo did not switch to its {} display within {} seconds.", if wanted { "inner" } else { "cover" }, SWITCH.as_secs()))?
        }
    };
    if switched.is_some() {
        let mut times = SWITCHED.lock().unwrap_or_else(PoisonError::into_inner);
        times.retain(|(u, _)| u != udid);
        times.push((udid.to_string(), Instant::now()));
    }
    Ok(switched)
}
/// The displays once the wanted one is active, `Some(None)` when overtaken, `None` after `limit`.
fn wait(udid: &str, inner: bool, limit: Duration, overtaken: &dyn Fn() -> bool) -> Option<Option<Vec<Display>>> {
    let deadline = Instant::now() + limit;
    loop {
        if overtaken() {
            return Some(None);
        }
        if let Ok(list) = displays(udid) {
            if active(&list).map(|d| d.inner) == Some(inner) {
                return Some(Some(list));
            }
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(SWITCH_POLL);
    }
}

/// Helper arguments for a touch at interface point (`x`, `y`) held `hold` seconds.
pub fn tap_args(display: &Display, (x, y): (f64, f64), hold: f64) -> Vec<String> {
    let (w, h) = display.ui();
    let (nx, ny) = to_native(display.rotation, x / w, y / h);
    vec!["tap".into(), display.uuid.clone(), format!("{nx:.5}"), format!("{ny:.5}"), format!("{hold:.2}")]
}
pub fn swipe_args(display: &Display, (x1, y1): (f64, f64), (x2, y2): (f64, f64), seconds: f64) -> Vec<String> {
    let (w, h) = display.ui();
    let (a, b) = (to_native(display.rotation, x1 / w, y1 / h), to_native(display.rotation, x2 / w, y2 / h));
    vec!["swipe".into(), display.uuid.clone(), format!("{:.5}", a.0), format!("{:.5}", a.1), format!("{:.5}", b.0), format!("{:.5}", b.1), format!("{seconds:.2}")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn types(ids: &[&str]) -> Value {
        json!({"devicetypes": ids.iter().map(|i| json!({"identifier": i})).collect::<Vec<_>>()})
    }
    fn runtimes(list: &[(&str, &str, bool)]) -> Value {
        json!({"runtimes": list.iter().map(|(id, v, ok)| json!({"identifier": id, "version": v, "isAvailable": ok, "platform": "iOS"})).collect::<Vec<_>>()})
    }
    const DUO: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-Duo";
    /// Trimmed `devicectl device info displays -j -` from Xcode 27.1, iPhone Duo open.
    fn devicectl(inner_active: bool, inner_rotation: &str) -> Value {
        json!({"info":{"outcome":"success"},"result":{"backlightState":"activeOn","displays":[
            {"active":!inner_active,"bounds":[[0,0],[1398,2034]],"currentOrientation":"rot0","displayId":1,"name":"LCD","nativeOrientation":"rot0","pointScale":3,"primary":true,"uniqueId":"44CBC3B3-9C06-47FC-A43A-5217DF5F24DF"},
            {"active":inner_active,"bounds":[[0,0],[2007,2853]],"currentOrientation":inner_rotation,"displayId":3,"name":"LCD-1","nativeOrientation":"rot0","pointScale":3,"primary":false,"uniqueId":"FC3D60D2-2B24-4468-98B0-0F3DA0D3B779"},
            {"active":false,"name":"TVOut","uniqueId":"x"}]}})
    }

    #[test]
    fn only_the_native_type_counts_and_there_is_no_ipad_fallback() {
        assert_eq!(pick_type(&types(&["com.apple.CoreSimulator.SimDeviceType.iPad-A16", DUO])).as_deref(), Some(DUO));
        assert_eq!(pick_type(&types(&["com.apple.CoreSimulator.SimDeviceType.V68"])).as_deref(), Some("com.apple.CoreSimulator.SimDeviceType.V68"));
        assert_eq!(pick_type(&types(&["com.apple.CoreSimulator.SimDeviceType.iPad-A16"])), None);
    }
    #[test]
    fn duo_needs_a_27_1_runtime() {
        let (old, new, newer) = (("a", "27.0", true), ("b", "27.1", true), ("c", "27.2", true));
        assert!(!available(&types(&[DUO]), &runtimes(&[old])));
        assert!(available(&types(&[DUO]), &runtimes(&[old, new])));
        assert!(!available(&types(&[]), &runtimes(&[new])));
        assert!(!available(&types(&[DUO]), &runtimes(&[("d", "27.1", false)])));
        assert_eq!(pick_runtime(&runtimes(&[new, newer, ("e", "28.0", false)])).as_deref(), Some("c"));
        assert_eq!(pick_runtime(&runtimes(&[("f", "26.10", true)])), None);
        assert!(UNAVAILABLE.contains("Xcode 27.1") && UNAVAILABLE.contains("macOS 26.6"));
    }
    #[test]
    fn poses_pick_their_display_and_the_device_wins_over_the_record() {
        assert_eq!(display_for(Pose::Closed), "primary");
        assert_eq!(display_for(Pose::Book), "primary-1");
        assert_eq!(serde_json::to_string(&Pose::Book).unwrap(), "\"book\"");
        assert!(serde_json::from_str::<Pose>("\"flat\"").is_err());
        assert_eq!([Pose::Closed, Pose::Book, Pose::Open].map(Pose::degrees), [0, 127, 180]);
        assert_eq!(reconcile(Pose::Book, Some(true)), Pose::Book);
        assert_eq!(reconcile(Pose::Closed, Some(true)), Pose::Open);
        assert_eq!(reconcile(Pose::Open, Some(false)), Pose::Closed);
        assert_eq!(reconcile(Pose::Book, None), Pose::Book);
    }
    #[test]
    fn devicectl_names_the_active_display_and_its_interface_size() {
        let open = parse_displays(&devicectl(true, "rot90"));
        assert_eq!(open.len(), 2);
        let inner = active(&open).unwrap();
        assert_eq!((inner.inner, inner.rotation, inner.native, inner.ui(), inner.name()), (true, 90, (669.0, 951.0), (951.0, 669.0), "inner"));
        assert_eq!((inner.uuid.as_str(), inner.id), ("FC3D60D2-2B24-4468-98B0-0F3DA0D3B779", 3));
        let closed = parse_displays(&devicectl(false, "rot90"));
        let cover = active(&closed).unwrap();
        assert_eq!((cover.inner, cover.ui(), cover.name()), (false, (466.0, 678.0), "cover"));
        assert!(parse_displays(&json!({"result":{}})).is_empty());
    }
    #[test]
    fn interface_points_map_to_native_axes_for_every_rotation() {
        assert_eq!(to_native(0, 0.25, 0.75), (0.25, 0.75));
        assert_eq!(to_native(90, 0.25, 0.75), (0.75, 0.75));
        assert_eq!(to_native(180, 0.25, 0.75), (0.75, 0.25));
        assert_eq!(to_native(270, 0.25, 0.75), (0.25, 0.25));
        // Verified live: the Settings icon at (501, 80) on the open inner display.
        let inner = active(&parse_displays(&devicectl(true, "rot90"))).cloned().unwrap();
        assert_eq!(tap_args(&inner, (501.0, 80.0), 0.08), ["tap", "FC3D60D2-2B24-4468-98B0-0F3DA0D3B779", "0.11958", "0.47319", "0.08"]);
        assert_eq!(swipe_args(&inner, (0.0, 669.0), (951.0, 0.0), 0.3)[2..], ["1.00000", "1.00000", "0.00000", "0.00000", "0.30"]);
        // A native rectangle lands where its points map.
        for rotation in [0, 90, 180, 270] {
            let native = (669.0, 951.0);
            let ui = if rotation % 180 == 90 { (951.0, 669.0) } else { native };
            let [x, y, w, h] = rect_to_ui(rotation, native, [52.0, 417.0, 86.0, 68.0]);
            let (cx, cy) = to_native(rotation, (x + w / 2.0) / ui.0, (y + h / 2.0) / ui.1);
            assert!((cx * native.0 - 95.0).abs() < 1e-9 && (cy * native.1 - 451.0).abs() < 1e-9, "rot{rotation}");
        }
        assert_eq!(rect_to_ui(90, (669.0, 951.0), [52.0, 417.0, 86.0, 68.0]), [466.0, 52.0, 68.0, 86.0]);
        // The helper's `elements` output (live, open home screen) lands in interface points.
        let found = parse_elements(r#"[{"label":"Settings","frame":[52,417,86,68],"ui":false},{"label":"About","frame":[340,303.5,527,52],"ui":true},{"label":"x","frame":[1,2]}]"#, &inner).unwrap();
        assert_eq!(found, [("Settings".to_string(), [466.0, 52.0, 68.0, 86.0]), ("About".to_string(), [340.0, 303.5, 527.0, 52.0])]);
        assert!(parse_elements("duoctl-helper: oops", &inner).is_err());
    }
    #[test]
    fn inner_display_is_read_landscape_from_capabilities() {
        // The shape of iPhone Duo.simdevicetype's capabilities.plist (Xcode 27.1).
        let caps = json!({"capabilities":{"HomeButtonType":2,"displays":[
            {"deviceName":"primary","displayName":"LCD","screenID":1,"width":1398,"height":2034,"scale":3,"nativeOrientation":0},
            {"deviceName":"primary-1","displayName":"LCD-1","screenID":3,"width":2007,"height":2853,"scale":3,"nativeOrientation":270,"nativeRotation":270},
            {"deviceName":"external-0","displayName":"TVOut","screenID":2,"width":720,"height":480,"scale":1}]},"iconState":{}});
        assert_eq!(parse_inner(&caps), Some(Size { width: 951.0, height: 669.0 }));
        assert_eq!(parse_display(&caps, COVER), Some((Size { width: 466.0, height: 678.0 }, 3.0)));
        assert_eq!(parse_inner(&json!({"displays": caps["capabilities"]["displays"]})), None);
        assert_eq!(parse_inner(&json!({"capabilities":{"displays":[{"deviceName":"primary-1"}]}})), None);
    }
    #[test]
    fn folds_run_one_at_a_time_and_only_the_newest_waiting_one_runs() {
        use std::{sync::Arc, thread};
        let latest = Arc::new(LatestWins::default());
        let (first, ran) = (latest.clone(), Arc::new(Mutex::new(Vec::new())));
        let log = ran.clone();
        let result = first.run(|overtaken| {
            assert!(!overtaken());
            // Two more arrive while the first holds the hinge.
            let waiting: Vec<_> = (2..=3)
                .map(|n| {
                    let (worker, log) = (latest.clone(), log.clone());
                    let handle = thread::spawn(move || worker.run(|_| log.lock().unwrap().push(n)));
                    while latest.seq.load(Ordering::SeqCst) < n {
                        thread::yield_now();
                    }
                    handle
                })
                .collect();
            assert!(overtaken(), "the running fold hears of the newer one");
            waiting
        });
        let outcomes: Vec<_> = result.unwrap().into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(outcomes, [None, Some(())], "the overtaken second request never runs");
        assert_eq!(*ran.lock().unwrap(), [3]);
    }
    #[test]
    fn the_helper_builds_for_the_simulator() {
        let args = build_args(Path::new("/c/h.m"), Path::new("/c/h.tmp"));
        assert_eq!(args.join(" "), "-sdk iphonesimulator clang -fobjc-arc -O2 -arch arm64 -mios-simulator-version-min=17.0 -framework Foundation -framework IOKit /c/h.m -o /c/h.tmp");
        assert!(HELPER.contains("Copyright (c) 2026 Karolina Samorek") && HELPER.contains("Permission is hereby granted") && HELPER.contains("hinge-sweep") && HELPER.contains("Indocode additions: `home`"));
    }

    /// Needs Xcode 27.1, AXe and a booted iPhone Duo: `cargo test -- --ignored live_duo_taps`
    #[test]
    #[ignore]
    fn live_duo_taps_by_label_and_point_on_the_inner_display() {
        use super::super::{describe, input, screenshot, Input, Simulators};
        let sims = Simulators::default();
        let device = sims.resolve(None, true).unwrap();
        assert!(device.duo, "boot the iPhone Duo first");
        let udid = device.udid.as_str();
        if active(&sims.duo(udid)).map(|d| d.inner) != Some(true) {
            sims.fold(udid, Pose::Open).unwrap();
        }
        let shot = || screenshot(udid, None, Some(INNER)).unwrap();
        let settings_center = || {
            let tree = describe(udid, None, &sims.duo(udid)).unwrap();
            let line = tree.lines().find(|l| l.contains("Button \"Settings\"")).unwrap_or_else(|| panic!("{tree}")).to_string();
            let center = line.split("center=(").nth(1).and_then(|r| r.split(')').next()).unwrap().to_string();
            let (x, y) = center.split_once(',').unwrap();
            (x.parse::<f64>().unwrap(), y.parse::<f64>().unwrap())
        };
        for by_label in [true, false] {
            xcrun(&["simctl", "terminate", udid, "com.apple.Preferences"], QUICK).ok();
            std::thread::sleep(Duration::from_secs(2));
            let (x, y) = settings_center();
            eprintln!("Settings on the inner display at ({x}, {y})");
            // The icon sits in the top half of the 951×669 landscape home screen, not at its far edge.
            assert!((300.0..700.0).contains(&x) && y < 200.0, "({x}, {y})");
            let before = shot();
            let action = if by_label { Input::TapLabel { label: "Settings".into() } } else { Input::Tap { x, y } };
            eprintln!("{}", input(udid, sims.shown(&device).unwrap(), &sims.duo(udid), &action).unwrap());
            std::thread::sleep(Duration::from_secs(2));
            assert_ne!(shot(), before, "the inner display changed");
            // Settings takes a moment to fill in its rows.
            let opened = (0..5).any(|_| describe(udid, None, &sims.duo(udid)).is_ok_and(|t| t.contains("\"General\"")) || { std::thread::sleep(Duration::from_secs(1)); false });
            assert!(opened, "Settings opened (by_label {by_label})");
        }
        xcrun(&["simctl", "terminate", udid, "com.apple.Preferences"], QUICK).ok();
    }

    /// Needs Xcode 27.1, AXe and a booted iPhone Duo: `cargo test -- --ignored duo`
    #[test]
    #[ignore]
    fn live_duo_folds_shows_both_displays_and_taps_the_inner_one() {
        use super::super::{describe, input, screenshot, Input, Simulators};
        let sims = Simulators::default();
        let device = sims.resolve(None, true).unwrap();
        let device = if device.duo { device } else { sims.resolve(Some(&create().unwrap()), true).unwrap() };
        let udid = device.udid.as_str();
        let shot = |sims: &Simulators| screenshot(udid, None, sims.display(udid)).unwrap();

        // Rapid cycles, as fast as the panel can ask: every close reaches the cover.
        for cycle in 0..10 {
            for pose in [Pose::Open, Pose::Closed] {
                let started = Instant::now();
                sims.fold(udid, pose).unwrap_or_else(|e| panic!("cycle {cycle} {pose:?}: {e}"));
                eprintln!("cycle {cycle} {pose:?}: {:?}", started.elapsed());
            }
        }
        // Fold to first frame, both ways, as the panel sees it.
        for pose in [Pose::Open, Pose::Closed] {
            let started = Instant::now();
            sims.fold(udid, pose).unwrap();
            let folded = started.elapsed();
            sims.streams.next(udid, sims.display(udid), 0, Duration::from_secs(10)).unwrap().expect("a frame of the new display");
            eprintln!("{pose:?}: folded in {folded:?}, first frame after {:?}", started.elapsed());
            assert!(started.elapsed() < Duration::from_secs(4), "{pose:?} took {:?}", started.elapsed());
        }
        assert_eq!((sims.pose(udid), sims.display(udid)), (Pose::Closed, Some(COVER)));
        let cover = shot(&sims);
        assert_eq!(&cover[1..4], b"PNG");
        // An all-black 1398×2034 PNG compresses to a few kilobytes.
        assert!(cover.len() > 200_000, "the cover screenshot has content ({} bytes)", cover.len());

        sims.fold(udid, Pose::Open).unwrap();
        assert_eq!((sims.pose(udid), sims.display(udid)), (Pose::Open, Some(INNER)));
        xcrun(&["simctl", "terminate", udid, "com.apple.Preferences"], QUICK).ok();
        std::thread::sleep(Duration::from_secs(2));
        let screen = sims.shown(&device).unwrap();
        assert_eq!((screen.width, screen.height), (951.0, 669.0));
        let duo = sims.duo(udid);
        let tree = describe(udid, None, &duo).unwrap();
        assert!(tree.contains("Screen 951×669"), "{tree}");
        let line = tree.lines().find(|l| l.contains("Button \"Settings\"")).unwrap_or_else(|| panic!("{tree}"));
        let center = line.split("center=(").nth(1).and_then(|r| r.split(')').next()).unwrap();
        let (x, y) = center.split_once(',').map(|(x, y)| (x.parse::<f64>().unwrap(), y.parse::<f64>().unwrap())).unwrap();
        let before = shot(&sims);
        input(udid, screen, &duo, &Input::Tap { x, y }).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert_ne!(shot(&sims), before, "tapping Settings at ({x}, {y}) changed the inner display");
        assert!(describe(udid, None, &sims.duo(udid)).unwrap().contains("General"), "Settings opened");

        sims.fold(udid, Pose::Book).unwrap();
        assert_eq!((sims.pose(udid), sims.display(udid)), (Pose::Book, Some(INNER)));
        sims.fold(udid, Pose::Open).unwrap();

        // Home from inside Settings on the inner display brings back the home screen.
        xcrun(&["simctl", "launch", udid, "com.apple.Preferences"], QUICK).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        let in_app = shot(&sims);
        input(udid, sims.shown(&device).unwrap(), &sims.duo(udid), &Input::Button { button: "home".into() }).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert_ne!(shot(&sims), in_app, "home changed the inner display");
        let home = describe(udid, None, &sims.duo(udid)).unwrap();
        // Not the dock's Safari: after a SpringBoard restart the dock is drawn but not in the accessibility tree.
        assert!(home.contains("Button \"Settings\"") && home.contains("id=Home screen icons") && !home.contains("\"General\""), "the home screen is back: {home}");
    }
}
