//! Network status for the NETWORK module of the bar (`render::ItemId::Network`) and its popup.
//!
//! COST: nothing here runs, loads or allocates until the module is enabled (`show_network`) AND the
//! visible bar asks for a status (`ShellRequest::NetworkQuery`, at most every `ui::NET_POLL_TICKS`
//! sysmon ticks). Then, on the shell thread only:
//!  * `iphlpapi.dll` (`GetAdaptersAddresses`) is loaded dynamically (`LoadLibraryExW`, System32 only):
//!    picks the active adapter (up, with a gateway, lowest IPv4 metric), its kind, name and IPv4;
//!  * `wlanapi.dll` is loaded ONLY while that adapter is Wi-Fi (SSID + signal quality) and unloaded
//!    as soon as it is not (ethernet / disconnected);
//!  * one `INetworkListManager` (COM, in-proc) answers "is there internet?" (`GetConnectivity`).
//!
//! `ShellRequest::NetworkRelease` (module disabled) drops the `Querier`: handles closed, COM object
//! released, libraries freed. No thread, no timer and no callback is ever registered here.
//!
//! THREADING: `Querier` lives on the shell thread (COM STA). `NetInfo`/`NetStatus` are plain data.

use std::ffi::c_void;

use windows::core::{w, GUID, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    GET_ADAPTERS_ADDRESSES_FLAGS, IF_TYPE_IEEE80211, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::NetworkManagement::WiFi::{
    wlan_interface_state_connected, wlan_intf_opcode_current_connection, WLAN_CONNECTION_ATTRIBUTES,
    WLAN_INTERFACE_INFO_LIST, WLAN_INTF_OPCODE, WLAN_OPCODE_VALUE_TYPE,
};
use windows::Win32::Networking::NetworkListManager::{
    INetworkListManager, NetworkListManager, NLM_CONNECTIVITY_IPV4_INTERNET, NLM_CONNECTIVITY_IPV6_INTERNET,
};
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

use crate::log;

/// Kind of the active connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetKind {
    /// No adapter with a gateway: offline (or not queried yet).
    #[default]
    None,
    Wifi,
    /// Wired, or any other non-Wi-Fi adapter with a gateway (USB tethering, cellular...).
    Ethernet,
}

/// Full status of the active connection (popup).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetInfo {
    pub kind: NetKind,
    /// SSID (Wi-Fi) or adapter friendly name (e.g. "Ethernet"); empty when offline.
    pub name: String,
    /// Wi-Fi signal quality 0..=100 (0 for other kinds).
    pub signal: u8,
    /// Local IPv4 in dotted form; empty if the adapter has none.
    pub ipv4: String,
    /// The network reaches the internet (NCSI result through the Network List Manager).
    pub internet: bool,
}

/// What the bar glyph shows: a `Copy` subset of `NetInfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetStatus {
    pub kind: NetKind,
    /// Wi-Fi bars 0..=4 (`bars_for`); 0 for other kinds.
    pub bars: u8,
    pub internet: bool,
}

impl NetInfo {
    pub fn status(&self) -> NetStatus {
        NetStatus {
            kind: self.kind,
            bars: if self.kind == NetKind::Wifi { bars_for(self.signal) } else { 0 },
            internet: self.internet,
        }
    }
}

/// Wi-Fi bars (1..=4) for a 0..=100 signal quality: a connected network always shows at least one.
pub fn bars_for(signal: u8) -> u8 {
    match signal {
        0..=24 => 1,
        25..=49 => 2,
        50..=74 => 3,
        _ => 4,
    }
}

// ------------------------------------------------------------------ dynamic libraries

/// A library loaded with `LoadLibraryExW`, freed on drop.
struct Lib(HMODULE);

impl Lib {
    fn load(name: PCWSTR) -> Option<Lib> {
        // SAFETY: `name` is a static wide literal; System32 only (no DLL planting).
        match unsafe { LoadLibraryExW(name, None, LOAD_LIBRARY_SEARCH_SYSTEM32) } {
            Ok(h) => Some(Lib(h)),
            Err(e) => {
                log!("net: LoadLibraryExW failed: {e}");
                None
            }
        }
    }

    /// Address of the export `name` (NUL-terminated ASCII), if present.
    fn proc(&self, name: &'static [u8]) -> Option<unsafe extern "system" fn() -> isize> {
        // SAFETY: the module is loaded (owned by `self`); `name` is a NUL-terminated literal.
        unsafe { GetProcAddress(self.0, PCSTR(name.as_ptr())) }
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        // SAFETY: handle from LoadLibraryExW, freed exactly once; no function pointer obtained from
        // it outlives the owner struct (they are dropped together).
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

type GaaFn = unsafe extern "system" fn(
    u32,
    GET_ADAPTERS_ADDRESSES_FLAGS,
    *const c_void,
    *mut IP_ADAPTER_ADDRESSES_LH,
    *mut u32,
) -> u32;
type WlanOpenFn = unsafe extern "system" fn(u32, *const c_void, *mut u32, *mut HANDLE) -> u32;
type WlanCloseFn = unsafe extern "system" fn(HANDLE, *const c_void) -> u32;
type WlanEnumFn = unsafe extern "system" fn(HANDLE, *const c_void, *mut *mut WLAN_INTERFACE_INFO_LIST) -> u32;
type WlanQueryFn = unsafe extern "system" fn(
    HANDLE,
    *const GUID,
    WLAN_INTF_OPCODE,
    *const c_void,
    *mut u32,
    *mut *mut c_void,
    *mut WLAN_OPCODE_VALUE_TYPE,
) -> u32;
type WlanFreeFn = unsafe extern "system" fn(*const c_void);

/// `iphlpapi.dll` with `GetAdaptersAddresses`.
struct IpHelper {
    gaa: GaaFn,
    _lib: Lib,
}

impl IpHelper {
    fn load() -> Option<IpHelper> {
        let lib = Lib::load(w!("iphlpapi.dll"))?;
        let p = lib.proc(b"GetAdaptersAddresses\0")?;
        // SAFETY: the export has exactly the `GaaFn` signature (iphlpapi.h).
        Some(IpHelper { gaa: unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, GaaFn>(p) }, _lib: lib })
    }
}

/// `wlanapi.dll` with an open client handle.
struct Wlan {
    handle: HANDLE,
    close: WlanCloseFn,
    enum_ifs: WlanEnumFn,
    query: WlanQueryFn,
    free: WlanFreeFn,
    _lib: Lib,
}

impl Wlan {
    fn open() -> Option<Wlan> {
        let lib = Lib::load(w!("wlanapi.dll"))?;
        let (o, c, e, q, f) = (
            lib.proc(b"WlanOpenHandle\0")?,
            lib.proc(b"WlanCloseHandle\0")?,
            lib.proc(b"WlanEnumInterfaces\0")?,
            lib.proc(b"WlanQueryInterface\0")?,
            lib.proc(b"WlanFreeMemory\0")?,
        );
        type P = unsafe extern "system" fn() -> isize;
        // SAFETY: each export has the signature declared by its type alias (wlanapi.h).
        let (open, close, enum_ifs, query, free) = unsafe {
            (
                std::mem::transmute::<P, WlanOpenFn>(o),
                std::mem::transmute::<P, WlanCloseFn>(c),
                std::mem::transmute::<P, WlanEnumFn>(e),
                std::mem::transmute::<P, WlanQueryFn>(q),
                std::mem::transmute::<P, WlanFreeFn>(f),
            )
        };
        let mut ver = 0u32;
        let mut handle = HANDLE::default();
        // SAFETY: out pointers to locals; client version 2 (Vista+).
        let r = unsafe { open(2, std::ptr::null(), &mut ver, &mut handle) };
        if r != 0 {
            log!("net: WlanOpenHandle failed ({r})");
            return None;
        }
        Some(Wlan { handle, close, enum_ifs, query, free, _lib: lib })
    }

    /// SSID and signal quality of the connected interface `guid` (string as in
    /// `IP_ADAPTER_ADDRESSES::AdapterName`, braces optional), or of the first connected one.
    fn current(&self, guid: &str) -> Option<(String, u8)> {
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        // SAFETY: valid handle; `list` receives memory owned by wlanapi, freed below.
        if unsafe { (self.enum_ifs)(self.handle, std::ptr::null(), &mut list) } != 0 || list.is_null() {
            return None;
        }
        // SAFETY: `list` points to a WLAN_INTERFACE_INFO_LIST with `dwNumberOfItems` trailing entries.
        let ifs = unsafe {
            let n = (*list).dwNumberOfItems as usize;
            std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), n)
        };
        let want = guid.trim_matches(|c| c == '{' || c == '}');
        let pick = ifs
            .iter()
            .find(|i| format!("{:?}", i.InterfaceGuid).eq_ignore_ascii_case(want))
            .or_else(|| ifs.iter().find(|i| i.isState == wlan_interface_state_connected))
            .map(|i| i.InterfaceGuid);
        let mut out = None;
        if let Some(g) = pick {
            let mut size = 0u32;
            let mut data: *mut c_void = std::ptr::null_mut();
            // SAFETY: valid handle and GUID; `data` receives wlanapi memory, freed below.
            let r = unsafe {
                (self.query)(
                    self.handle,
                    &g,
                    wlan_intf_opcode_current_connection,
                    std::ptr::null(),
                    &mut size,
                    &mut data,
                    std::ptr::null_mut(),
                )
            };
            if r == 0 && !data.is_null() && size as usize >= std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
                // SAFETY: for this opcode the data is a WLAN_CONNECTION_ATTRIBUTES (size checked).
                let a = unsafe { &*(data as *const WLAN_CONNECTION_ATTRIBUTES) };
                if a.isState == wlan_interface_state_connected {
                    let s = &a.wlanAssociationAttributes.dot11Ssid;
                    let len = (s.uSSIDLength as usize).min(s.ucSSID.len());
                    let ssid = String::from_utf8_lossy(&s.ucSSID[..len]).into_owned();
                    out = Some((ssid, a.wlanAssociationAttributes.wlanSignalQuality.min(100) as u8));
                }
            }
            if !data.is_null() {
                // SAFETY: memory returned by WlanQueryInterface.
                unsafe { (self.free)(data) };
            }
        }
        // SAFETY: memory returned by WlanEnumInterfaces; `ifs` is not used after this.
        unsafe { (self.free)(list as *const c_void) };
        out
    }
}

impl Drop for Wlan {
    fn drop(&mut self) {
        // SAFETY: handle opened by WlanOpenHandle; closed before the library is freed (`_lib` drops last).
        unsafe { (self.close)(self.handle, std::ptr::null()) };
    }
}

// ------------------------------------------------------------------ querier

/// Initial size of the `GetAdaptersAddresses` buffer (bytes); grown on demand and reused.
const GAA_BUF_BYTES: usize = 16 * 1024;
/// `ERROR_BUFFER_OVERFLOW`.
const ERROR_BUFFER_OVERFLOW: u32 = 111;
/// Interface types never considered the "active connection": loopback and tunnels.
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
const IF_TYPE_TUNNEL: u32 = 131;

/// Shell-thread state of the network module: created on the first `NetworkQuery`, dropped on
/// `NetworkRelease` (everything it holds is freed by `Drop`).
#[derive(Default)]
pub struct Querier {
    ip: Option<IpHelper>,
    /// Open only while the active adapter is Wi-Fi.
    wlan: Option<Wlan>,
    nlm: Option<INetworkListManager>,
    /// The NLM could not be created: do not retry on every poll.
    nlm_failed: bool,
    /// `Wlan::open` failed (WLAN service stopped/disabled): not retried until the adapter leaves Wi-Fi.
    wlan_failed: bool,
    /// `GetAdaptersAddresses` buffer (u64 for the 8-byte alignment of the structures), reused.
    buf: Vec<u64>,
}

/// The adapter chosen as the active connection.
struct Adapter {
    kind: NetKind,
    name: String,
    guid: String,
    ipv4: String,
}

impl Querier {
    pub fn new() -> Querier {
        log!("net: network module active (querier created)");
        Querier::default()
    }

    /// Current status. Never blocks for long: local IP Helper / WLAN service / NLM calls only.
    pub fn query(&mut self) -> NetInfo {
        let mut info = NetInfo::default();
        let Some(a) = self.active_adapter() else {
            self.wlan = None;
            self.wlan_failed = false;
            return info;
        };
        info.kind = a.kind;
        info.name = a.name;
        info.ipv4 = a.ipv4;
        if a.kind == NetKind::Wifi {
            if self.wlan.is_none() && !self.wlan_failed {
                self.wlan = Wlan::open();
                self.wlan_failed = self.wlan.is_none();
            }
            if let Some((ssid, q)) = self.wlan.as_ref().and_then(|w| w.current(&a.guid)) {
                if !ssid.is_empty() {
                    info.name = ssid;
                }
                info.signal = q;
            }
        } else {
            // Not on Wi-Fi: wlanapi is not needed, release it.
            self.wlan = None;
            self.wlan_failed = false;
        }
        info.internet = self.internet().unwrap_or(true);
        info
    }

    /// Adapter that is up, has a gateway and an IPv4 address, with the lowest IPv4 metric.
    fn active_adapter(&mut self) -> Option<Adapter> {
        if self.ip.is_none() {
            self.ip = IpHelper::load();
        }
        let gaa = self.ip.as_ref()?.gaa;
        if self.buf.is_empty() {
            self.buf = vec![0u64; GAA_BUF_BYTES / 8];
        }
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_INCLUDE_GATEWAYS;
        let mut ok = false;
        for _ in 0..3 {
            let mut size = (self.buf.len() * 8) as u32;
            // SAFETY: `buf` is writable for `size` bytes and 8-byte aligned; AF_UNSPEC = 0.
            let r = unsafe {
                gaa(0, flags, std::ptr::null(), self.buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(), &mut size)
            };
            if r == 0 {
                ok = true;
                break;
            }
            if r != ERROR_BUFFER_OVERFLOW {
                log!("net: GetAdaptersAddresses failed ({r})");
                return None;
            }
            self.buf = vec![0u64; (size as usize).div_ceil(8) + 64];
        }
        if !ok {
            return None;
        }
        let mut best: Option<(u32, Adapter)> = None;
        let mut p = self.buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !p.is_null() {
            // SAFETY: `p` walks the linked list written by GetAdaptersAddresses inside `buf`.
            let a = unsafe { &*p };
            p = a.Next;
            if a.OperStatus != IfOperStatusUp
                || a.FirstGatewayAddress.is_null()
                || a.IfType == IF_TYPE_SOFTWARE_LOOPBACK
                || a.IfType == IF_TYPE_TUNNEL
            {
                continue;
            }
            let Some(ipv4) = first_ipv4(a) else { continue };
            if best.as_ref().is_some_and(|(m, _)| *m <= a.Ipv4Metric) {
                continue;
            }
            // SAFETY: FriendlyName is a NUL-terminated wide string inside `buf` (or null).
            let name = unsafe { if a.FriendlyName.is_null() { String::new() } else { a.FriendlyName.to_string().unwrap_or_default() } };
            // SAFETY: AdapterName is a NUL-terminated ANSI string inside `buf` (or null).
            let guid = unsafe { if a.AdapterName.is_null() { String::new() } else { a.AdapterName.to_string().unwrap_or_default() } };
            let kind = if a.IfType == IF_TYPE_IEEE80211 { NetKind::Wifi } else { NetKind::Ethernet };
            best = Some((a.Ipv4Metric, Adapter { kind, name, guid, ipv4 }));
        }
        best.map(|(_, a)| a)
    }

    /// IPv4/IPv6 internet connectivity from the Network List Manager (`None` = unknown).
    fn internet(&mut self) -> Option<bool> {
        if self.nlm.is_none() && !self.nlm_failed {
            // SAFETY: COM is initialized (STA) on the shell thread, the only caller.
            match unsafe { CoCreateInstance::<_, INetworkListManager>(&NetworkListManager, None, CLSCTX_ALL) } {
                Ok(n) => self.nlm = Some(n),
                Err(e) => {
                    log!("net: NetworkListManager not available: {e}");
                    self.nlm_failed = true;
                }
            }
        }
        let nlm = self.nlm.as_ref()?;
        // SAFETY: valid interface pointer on its own apartment thread.
        match unsafe { nlm.GetConnectivity() } {
            Ok(c) => Some(c.0 & (NLM_CONNECTIVITY_IPV4_INTERNET.0 | NLM_CONNECTIVITY_IPV6_INTERNET.0) != 0),
            Err(e) => {
                log!("net: GetConnectivity failed: {e}");
                None
            }
        }
    }
}

impl Drop for Querier {
    fn drop(&mut self) {
        log!("net: network module released");
    }
}

/// First IPv4 unicast address of the adapter, dotted.
fn first_ipv4(a: &IP_ADAPTER_ADDRESSES_LH) -> Option<String> {
    let mut u = a.FirstUnicastAddress;
    while !u.is_null() {
        // SAFETY: linked list inside the GetAdaptersAddresses buffer.
        let ua = unsafe { &*u };
        u = ua.Next;
        let sa = ua.Address.lpSockaddr;
        if sa.is_null() || (ua.Address.iSockaddrLength as usize) < std::mem::size_of::<SOCKADDR_IN>() {
            continue;
        }
        // SAFETY: `sa` points to at least a SOCKADDR_IN (length checked); read the family first.
        let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
        if sin.sin_family != AF_INET {
            continue;
        }
        // SAFETY: plain union read of the 4 address bytes (network order in memory).
        let b = unsafe { sin.sin_addr.S_un.S_addr }.to_ne_bytes();
        return Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bars_and_status() {
        assert_eq!(bars_for(0), 1);
        assert_eq!(bars_for(49), 2);
        assert_eq!(bars_for(100), 4);
        let i = NetInfo { kind: NetKind::Ethernet, signal: 90, internet: true, ..Default::default() };
        assert_eq!(i.status(), NetStatus { kind: NetKind::Ethernet, bars: 0, internet: true });
        assert_eq!(NetInfo::default().status().kind, NetKind::None);
    }
}
