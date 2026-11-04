//! Drives come and go: a thread lists them again every couple of seconds so
//! the sidebar picks up ones plugged in later, and removable ones can be
//! ejected (unmounted and powered off, so they're safe to unplug).

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use egui::Context;

use crate::{
    phones::Phones,
    places::{self, Place},
};

/// Also how soon file watches let go of a memory card that was pulled out,
/// which Windows needs before it can mount the next one.
const POLL: Duration = Duration::from_secs(1);

pub struct Drives {
    list: Arc<Mutex<Vec<Place>>>,
    pub phones: Phones,
}

impl Drives {
    pub fn new(ctx: &Context) -> Self {
        let phones = Phones::new(ctx);
        let list = Arc::new(Mutex::new(places::drives()));
        let shared = list.clone();
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("drives".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(POLL);
                    let drives = places::drives();
                    let mut list = shared.lock().unwrap();
                    if *list != drives {
                        *list = drives;
                        ctx.request_repaint();
                    }
                }
            });
        Self { list, phones }
    }

    pub fn get(&self) -> Vec<Place> {
        let mut drives = self.list.lock().unwrap().clone();
        drives.extend(self.phones.places());
        drives
    }

    /// The drives to list, leaving out the ones without media if
    /// `hide_empty`.
    pub fn shown(&self, hide_empty: bool) -> Vec<Place> {
        let mut drives = self.get();
        drives.retain(|d| !hide_empty || d.media.is_some());
        drives
    }
}

/// Unmounts the drive mounted at `path` through UDisks, locks it if it was
/// encrypted, and powers it off (or ejects the disc).
#[cfg(target_os = "linux")]
pub fn eject(path: &Path) -> Result<(), String> {
    if let Some(result) = crate::phones::eject(path) {
        return result;
    }
    zbus::block_on(udisks::eject(path)).map_err(|err| match err {
        zbus::Error::MethodError(_, Some(message), _) => message,
        err => err.to_string(),
    })
}

#[cfg(target_os = "linux")]
mod udisks {
    use std::{collections::HashMap, os::unix::ffi::OsStrExt, path::Path};

    use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Variant};

    type Interfaces = HashMap<String, HashMap<String, OwnedVariant>>;
    type Objects = HashMap<OwnedObjectPath, Interfaces>;

    const SERVICE: &str = "org.freedesktop.UDisks2";

    pub async fn eject(path: &Path) -> zbus::Result<()> {
        let connection = Connection::system().await?;
        let manager = Proxy::new(
            &connection,
            SERVICE,
            "/org/freedesktop/UDisks2",
            "org.freedesktop.DBus.ObjectManager",
        )
        .await?;
        let objects: Objects = manager.call("GetManagedObjects", &()).await?;
        let Some((filesystem, interfaces)) = mounted_at(&objects, path) else {
            return Err(zbus::Error::Failure(format!(
                "{} isn't a mounted drive",
                path.display()
            )));
        };
        let no_options = HashMap::<&str, Variant>::new();

        call(
            &connection,
            filesystem,
            "Filesystem",
            "Unmount",
            &no_options,
        )
        .await?;

        let block = &interfaces[&iface("Block")];
        let backing = object_path(block, "CryptoBackingDevice").filter(|p| p.as_str() != "/");
        let drive_of = |block: &HashMap<String, OwnedVariant>| object_path(block, "Drive");
        let drive = if let Some(backing) = &backing {
            call(&connection, backing, "Encrypted", "Lock", &no_options).await?;
            objects
                .get(backing)
                .and_then(|i| i.get(&iface("Block")))
                .and_then(drive_of)
        } else {
            drive_of(block)
        };

        // Unmounted is what keeps the data safe; powering off is a courtesy
        // that fails while another partition of the drive is still mounted.
        if let Some(drive) = drive.filter(|p| p.as_str() != "/")
            && let Some(props) = objects.get(&drive).and_then(|i| i.get(&iface("Drive")))
        {
            let flag = |name| props.get(name).and_then(|v| bool::try_from(v).ok()) == Some(true);
            let method = if flag("CanPowerOff") {
                Some("PowerOff")
            } else if flag("Ejectable") {
                Some("Eject")
            } else {
                None
            };
            if let Some(method) = method {
                let _ = call(&connection, &drive, "Drive", method, &no_options).await;
            }
        }
        Ok(())
    }

    pub fn mounted_at<'a>(
        objects: &'a Objects,
        path: &Path,
    ) -> Option<(&'a OwnedObjectPath, &'a Interfaces)> {
        let wanted = path.as_os_str().as_bytes();
        objects.iter().find(|(_, interfaces)| {
            interfaces
                .get(&iface("Filesystem"))
                .and_then(|fs| fs.get("MountPoints"))
                .and_then(|v| Vec::<Vec<u8>>::try_from(v.try_clone().ok()?).ok())
                .is_some_and(|points| {
                    points
                        .iter()
                        .any(|p| p.strip_suffix(&[0]).unwrap_or(p) == wanted)
                })
        })
    }

    async fn call(
        connection: &Connection,
        object: &OwnedObjectPath,
        interface: &str,
        method: &str,
        options: &HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<()> {
        let proxy = Proxy::new(connection, SERVICE, object.as_str(), iface(interface)).await?;
        proxy.call_method(method, &(options,)).await?;
        Ok(())
    }

    fn iface(name: &str) -> String {
        format!("org.freedesktop.UDisks2.{name}")
    }

    fn object_path(props: &HashMap<String, OwnedVariant>, name: &str) -> Option<OwnedObjectPath> {
        OwnedObjectPath::try_from(props.get(name)?.try_clone().ok()?).ok()
    }

    /// Reads the real UDisks objects and finds the filesystem mounted at
    /// `EGUI_FILES_MOUNT` (default `/`).
    /// `cargo test udisks_finds_mounts -- --ignored --nocapture`
    #[cfg(test)]
    #[test]
    #[ignore]
    fn udisks_finds_mounts() {
        let objects: Objects = zbus::block_on(async {
            let connection = Connection::system().await?;
            let manager = Proxy::new(
                &connection,
                SERVICE,
                "/org/freedesktop/UDisks2",
                "org.freedesktop.DBus.ObjectManager",
            )
            .await?;
            manager.call("GetManagedObjects", &()).await
        })
        .unwrap();
        let mount = std::env::var("EGUI_FILES_MOUNT").unwrap_or_else(|_| "/".into());
        let (path, interfaces) = mounted_at(&objects, Path::new(&mount)).expect("mount found");
        let block = &interfaces[&iface("Block")];
        println!("{mount}: {path} on drive {:?}", object_path(block, "Drive"));
    }
}

/// Ejects the media in the drive at `path`, like Explorer's "Eject": a memory
/// card, USB stick or disc is let go of while the reader stays usable. A USB
/// hard drive, which Windows calls fixed, is "safely removed" instead.
#[cfg(windows)]
pub fn eject(path: &Path) -> Result<(), String> {
    windows_eject::eject(path)
}

#[cfg(windows)]
mod windows_eject {
    use std::{os::windows::ffi::OsStrExt, path::Path};

    use windows::{
        Win32::{
            Devices::DeviceAndDriverInstallation::{
                CM_Get_Parent, CM_Request_Device_EjectW, CR_SUCCESS, DIGCF_DEVICEINTERFACE,
                DIGCF_PRESENT, PNP_VETO_TYPE, PNP_VetoTypeUnknown, SP_DEVICE_INTERFACE_DATA,
                SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA, SetupDiDestroyDeviceInfoList,
                SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
                SetupDiGetDeviceInterfaceDetailW,
            },
            Foundation::{CloseHandle, HANDLE},
            Storage::FileSystem::{
                CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
                FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, OPEN_EXISTING,
            },
            System::{
                IO::DeviceIoControl,
                Ioctl::{
                    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, GUID_DEVINTERFACE_DISK,
                    IOCTL_STORAGE_EJECT_MEDIA, IOCTL_STORAGE_GET_DEVICE_NUMBER,
                    IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL, STORAGE_DEVICE_NUMBER,
                },
                WindowsProgramming::DRIVE_FIXED,
            },
        },
        core::PCWSTR,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    struct Handle(HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateFileW and is closed once.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn open(device: &[u16], access: u32) -> windows::core::Result<Handle> {
        // SAFETY: `device` is NUL-terminated.
        unsafe {
            CreateFileW(
                PCWSTR(device.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map(Handle)
    }

    fn device_number(handle: &Handle) -> windows::core::Result<STORAGE_DEVICE_NUMBER> {
        let mut number = STORAGE_DEVICE_NUMBER::default();
        // SAFETY: the output buffer is a STORAGE_DEVICE_NUMBER of the given size.
        unsafe {
            DeviceIoControl(
                handle.0,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                None,
                0,
                Some((&raw mut number).cast()),
                size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                None,
                None,
            )?;
        }
        Ok(number)
    }

    pub fn eject(path: &Path) -> Result<(), String> {
        let root: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let letter = path.to_string_lossy().chars().next().ok_or("not a drive")?;
        let volume = wide(&format!(r"\\.\{letter}:"));

        // Removing the whole device would take a card reader with it, and
        // Windows wouldn't see another card until it was plugged in again.
        // SAFETY: `root` is NUL-terminated.
        if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != DRIVE_FIXED {
            return eject_media(&volume);
        }

        let number = {
            let handle = open(&volume, 0).map_err(|e| e.message())?;
            device_number(&handle).map_err(|e| e.message())?
        };
        let disk = disk_instance(number.DeviceNumber).ok_or("the drive's disk wasn't found")?;
        // The disk's parent is the USB (or other bus) device to remove.
        let mut parent = 0;
        // SAFETY: `parent` receives the parent instance.
        if unsafe { CM_Get_Parent(&mut parent, disk, 0) } != CR_SUCCESS {
            return Err("the drive's device wasn't found".into());
        }
        // A veto says which program or driver has the drive in use; tries
        // twice because Windows sometimes refuses while it flushes caches.
        let mut veto_name = [0u16; 260];
        for attempt in 0..2 {
            let mut veto = PNP_VetoTypeUnknown;
            // SAFETY: the veto outputs are a PNP_VETO_TYPE and a buffer.
            let result = unsafe {
                CM_Request_Device_EjectW(
                    parent,
                    Some(&mut veto as *mut PNP_VETO_TYPE),
                    Some(&mut veto_name),
                    0,
                )
            };
            if result == CR_SUCCESS && veto == PNP_VetoTypeUnknown {
                return Ok(());
            }
            if attempt == 0 {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        let len = veto_name.iter().position(|&c| c == 0).unwrap_or(0);
        let name = String::from_utf16_lossy(&veto_name[..len]);
        Err(if name.is_empty() {
            "the drive is in use".into()
        } else {
            format!("the drive is in use ({name})")
        })
    }

    /// Locks and dismounts the volume, so nothing is left unwritten and
    /// nobody holds it, then ejects its media. The lock goes with the handle.
    fn eject_media(volume: &[u16]) -> Result<(), String> {
        let handle =
            open(volume, FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0).map_err(|e| e.message())?;
        // SAFETY: each call gets the buffer and size its control code takes.
        unsafe {
            let control = |code, input: Option<(*const std::ffi::c_void, u32)>| {
                DeviceIoControl(
                    handle.0,
                    code,
                    input.map(|(p, _)| p),
                    input.map_or(0, |(_, n)| n),
                    None,
                    0,
                    None,
                    None,
                )
            };
            // Programs that just finished with the drive may take a moment.
            let mut locked = control(FSCTL_LOCK_VOLUME, None);
            for _ in 0..10 {
                if locked.is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
                locked = control(FSCTL_LOCK_VOLUME, None);
            }
            locked.map_err(|_| "the drive is in use".to_owned())?;
            control(FSCTL_DISMOUNT_VOLUME, None).map_err(|e| e.message())?;
            let allow = PREVENT_MEDIA_REMOVAL {
                PreventMediaRemoval: false,
            };
            let allow = (
                (&raw const allow).cast(),
                size_of::<PREVENT_MEDIA_REMOVAL>() as u32,
            );
            // Not every reader supports these; dismounted is safe to pull.
            let _ = control(IOCTL_STORAGE_MEDIA_REMOVAL, Some(allow));
            let _ = control(IOCTL_STORAGE_EJECT_MEDIA, None);
        }
        Ok(())
    }

    /// The device instance of the disk with this number.
    fn disk_instance(number: u32) -> Option<u32> {
        // SAFETY: the device list is destroyed before returning; the detail
        // buffer is u32-aligned and as large as SetupDi asked for.
        unsafe {
            let set = SetupDiGetClassDevsW(
                Some(&GUID_DEVINTERFACE_DISK),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
            .ok()?;
            let mut found = None;
            for index in 0.. {
                let mut interface = SP_DEVICE_INTERFACE_DATA {
                    cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInterfaces(
                    set,
                    None,
                    &GUID_DEVINTERFACE_DISK,
                    index,
                    &mut interface,
                )
                .is_err()
                {
                    break;
                }
                let mut size = 0;
                let _ = SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    None,
                    0,
                    Some(&mut size),
                    None,
                );
                let mut buffer = vec![0u32; (size as usize).div_ceil(4).max(2)];
                let detail = buffer
                    .as_mut_ptr()
                    .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
                (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
                let mut info = SP_DEVINFO_DATA {
                    cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiGetDeviceInterfaceDetailW(
                    set,
                    &interface,
                    Some(detail),
                    size,
                    None,
                    Some(&mut info),
                )
                .is_err()
                {
                    continue;
                }
                // DevicePath runs from after `cbSize` to the end of `size`.
                let device_path = (&raw const (*detail).DevicePath).cast::<u16>();
                let len = (size as usize).saturating_sub(4) / 2;
                let Ok(handle) = open(std::slice::from_raw_parts(device_path, len), 0) else {
                    continue;
                };
                if device_number(&handle).is_ok_and(|n| n.DeviceNumber == number) {
                    found = Some(info.DevInst);
                    break;
                }
            }
            let _ = SetupDiDestroyDeviceInfoList(set);
            found
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn eject(_path: &Path) -> Result<(), String> {
    Err("ejecting isn't supported on this platform".into())
}
