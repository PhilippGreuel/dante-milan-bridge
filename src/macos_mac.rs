//! macOS: echte MAC-Adressen über IOKit lesen.
//!
//! Neuere macOS-Versionen liefern über getifaddrs()/sysctl (und damit über `netdev`) für
//! normale Prozesse nur noch den Platzhalter 02:00:00:00:00:00. Die IOKit-Registry
//! (Property "IOMACAddress" des IOEthernetController oberhalb des IOEthernetInterface)
//! enthält weiterhin die tatsächliche Adresse – dieselbe, die `ifconfig` anzeigt.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type IoObject = u32;

const UTF8: u32 = 0x0800_0100;
const ITERATE_RECURSIVELY: u32 = 1;
const ITERATE_PARENTS: u32 = 2;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(main_port: u32, matching: *mut c_void, it: *mut IoObject) -> i32;
    fn IOIteratorNext(it: IoObject) -> IoObject;
    fn IOObjectRelease(o: IoObject) -> i32;
    fn IORegistryEntryCreateCFProperty(e: IoObject, key: CFStringRef, a: CFAllocatorRef, opt: u32) -> CFTypeRef;
    fn IORegistryEntrySearchCFProperty(
        e: IoObject,
        plane: *const c_char,
        key: CFStringRef,
        a: CFAllocatorRef,
        opt: u32,
    ) -> CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(a: CFAllocatorRef, s: *const c_char, enc: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, len: CFIndex, enc: u32) -> u8;
    fn CFGetTypeID(o: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFDataGetLength(d: CFTypeRef) -> CFIndex;
    fn CFDataGetBytePtr(d: CFTypeRef) -> *const u8;
    fn CFRelease(o: CFTypeRef);
}

/// BSD-Name (z.B. "en0") -> MAC für alle Ethernet-artigen Schnittstellen.
pub fn mac_addresses() -> HashMap<String, [u8; 6]> {
    let mut out = HashMap::new();
    unsafe {
        let key_bsd = CFStringCreateWithCString(std::ptr::null(), c"BSD Name".as_ptr(), UTF8);
        let key_mac = CFStringCreateWithCString(std::ptr::null(), c"IOMACAddress".as_ptr(), UTF8);
        let mut it: IoObject = 0;
        // matching-Dictionary wird von IOServiceGetMatchingServices freigegeben
        if IOServiceGetMatchingServices(0, IOServiceMatching(c"IOEthernetInterface".as_ptr()), &mut it) == 0 {
            loop {
                let svc = IOIteratorNext(it);
                if svc == 0 {
                    break;
                }
                let bsd = IORegistryEntryCreateCFProperty(svc, key_bsd, std::ptr::null(), 0);
                let mac = IORegistryEntrySearchCFProperty(
                    svc,
                    c"IOService".as_ptr(),
                    key_mac,
                    std::ptr::null(),
                    ITERATE_RECURSIVELY | ITERATE_PARENTS,
                );
                if !bsd.is_null()
                    && !mac.is_null()
                    && CFGetTypeID(bsd) == CFStringGetTypeID()
                    && CFGetTypeID(mac) == CFDataGetTypeID()
                    && CFDataGetLength(mac) == 6
                {
                    let mut buf = [0 as c_char; 64];
                    if CFStringGetCString(bsd, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) != 0 {
                        let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
                        let mut m = [0u8; 6];
                        m.copy_from_slice(std::slice::from_raw_parts(CFDataGetBytePtr(mac), 6));
                        out.insert(name, m);
                    }
                }
                if !bsd.is_null() {
                    CFRelease(bsd);
                }
                if !mac.is_null() {
                    CFRelease(mac);
                }
                IOObjectRelease(svc);
            }
            IOObjectRelease(it);
        }
        CFRelease(key_bsd);
        CFRelease(key_mac);
    }
    out
}
