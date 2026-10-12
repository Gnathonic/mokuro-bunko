//! The device catalog: every physical device the system's Vulkan loader reports, with
//! the facts the host routes and caches by (vendor/device ids, driver, VRAM, UUID, PCI).
//! The loader is opened at run time; a machine without one has an empty catalog and
//! says why.

use std::ffi::CStr;

use ash::vk;

use crate::backend::{DeviceKind, VkDevice};

/// The catalog, in `vkEnumeratePhysicalDevices` order, software rasterizers included
/// (as [`DeviceKind::Cpu`]: the runtimes skip them).
pub fn probe() -> Result<Vec<VkDevice>, String> {
    // SAFETY: loads the system Vulkan loader; its initialisers are the loader's.
    let entry = unsafe { ash::Entry::load() }
        .map_err(|e| format!("no Vulkan loader (libvulkan / vulkan-1.dll): {e}"))?;
    let app = vk::ApplicationInfo::default()
        .application_name(c"mokuro-bunko")
        .api_version(vk::API_VERSION_1_1);
    let ci = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: valid create info; the instance is destroyed below.
    let inst = unsafe { entry.create_instance(&ci, None) }
        .map_err(|e| format!("vkCreateInstance failed: {e}"))?;
    let out = list(&inst);
    // SAFETY: nothing created from the instance outlives this call.
    unsafe { inst.destroy_instance(None) };
    out
}

fn list(inst: &ash::Instance) -> Result<Vec<VkDevice>, String> {
    // SAFETY: a live instance.
    let pds = unsafe { inst.enumerate_physical_devices() }
        .map_err(|e| format!("vkEnumeratePhysicalDevices failed: {e}"))?;
    Ok(pds
        .iter()
        .enumerate()
        .map(|(i, &pd)| describe(inst, i, pd))
        .collect())
}

fn has_ext(inst: &ash::Instance, pd: vk::PhysicalDevice, name: &CStr) -> bool {
    // SAFETY: a physical device of this instance.
    unsafe { inst.enumerate_device_extension_properties(pd) }
        .unwrap_or_default()
        .iter()
        .any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == name))
}

fn version(v: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(v),
        vk::api_version_minor(v),
        vk::api_version_patch(v)
    )
}

fn describe(inst: &ash::Instance, index: usize, pd: vk::PhysicalDevice) -> VkDevice {
    // SAFETY: a physical device of this instance.
    let props = unsafe { inst.get_physical_device_properties(pd) };
    let name = props
        .device_name_as_c_str()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    let kind = match props.device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => DeviceKind::Discrete,
        vk::PhysicalDeviceType::INTEGRATED_GPU => DeviceKind::Integrated,
        vk::PhysicalDeviceType::VIRTUAL_GPU => DeviceKind::Virtual,
        vk::PhysicalDeviceType::CPU => DeviceKind::Cpu,
        _ => DeviceKind::Other,
    };
    // SAFETY: as above.
    let mem = unsafe { inst.get_physical_device_memory_properties(pd) };
    let vram_mb = mem
        .memory_heaps_as_slice()
        .iter()
        .filter(|h| h.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
        .map(|h| h.size / (1024 * 1024))
        .max()
        .unwrap_or(0);
    let mut dev = VkDevice {
        index,
        name,
        vendor_id: props.vendor_id,
        device_id: props.device_id,
        kind,
        driver: String::new(),
        api_version: version(props.api_version),
        vram_mb,
        uuid: String::new(),
        pci: None,
    };
    if props.api_version < vk::API_VERSION_1_1 {
        dev.driver = format!("driver {:#x}", props.driver_version);
        return dev;
    }
    let driver_props = props.api_version >= vk::API_VERSION_1_2
        || has_ext(inst, pd, vk::KHR_DRIVER_PROPERTIES_NAME);
    let pci_info = has_ext(inst, pd, vk::EXT_PCI_BUS_INFO_NAME);
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut drv = vk::PhysicalDeviceDriverProperties::default();
    let mut pci = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
    {
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        if driver_props {
            p2 = p2.push_next(&mut drv);
        }
        if pci_info {
            p2 = p2.push_next(&mut pci);
        }
        // SAFETY: a Vulkan 1.1 device; the chained structs outlive the call.
        unsafe { inst.get_physical_device_properties2(pd, &mut p2) };
    }
    dev.uuid = id.device_uuid.iter().map(|b| format!("{b:02x}")).collect();
    dev.driver = if driver_props {
        let n = drv
            .driver_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        let i = drv
            .driver_info_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{n} {i}").trim().to_string()
    } else {
        format!("driver {:#x}", props.driver_version)
    };
    if pci_info {
        dev.pci = Some(format!(
            "{:04x}:{:02x}:{:02x}.{:x}",
            pci.pci_domain, pci.pci_bus, pci.pci_device, pci.pci_function
        ));
    }
    dev
}

/// Whether the driver is Mesa's RADV (the driver the AMD route is measured on).
pub fn is_radv(d: &VkDevice) -> bool {
    d.driver.to_ascii_lowercase().starts_with("radv") || d.name.contains("(RADV")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On a machine with a Vulkan loader every hardware device has the facts the host
    /// keys on; without one the error says so.
    #[test]
    fn the_catalog_has_identities() {
        match probe() {
            Ok(list) => {
                for d in list.iter().filter(|d| d.kind != DeviceKind::Cpu) {
                    eprintln!("{d:?}");
                    assert_eq!(d.uuid.len(), 32, "{d:?}");
                    assert!(!d.name.is_empty() && d.vram_mb > 0, "{d:?}");
                }
            }
            Err(e) => assert!(e.contains("Vulkan"), "{e}"),
        }
    }
}
