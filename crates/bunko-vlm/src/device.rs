//! Where a model runs: `cpu` or `gpu:<n>` (spec §7.3 device ids), whatever the backend.

use std::fmt;
use std::str::FromStr;

use crate::VlmError;

/// Where a model runs: `cpu` or `gpu:<n>` (spec §7.3 device ids; `gpu`/`cuda` = `gpu:0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Device {
    Cpu,
    Gpu(u8),
}

impl FromStr for Device {
    type Err = VlmError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "cpu" => return Ok(Device::Cpu),
            "gpu" | "cuda" => return Ok(Device::Gpu(0)),
            _ => {}
        }
        let n = s.strip_prefix("gpu:").or_else(|| s.strip_prefix("cuda:"));
        match n.and_then(|n| n.parse::<u8>().ok()) {
            Some(n) if n <= 15 => Ok(Device::Gpu(n)),
            _ => Err(VlmError::Config(format!(
                "unknown device {s:?} (cpu, gpu:<n>)"
            ))),
        }
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Device::Cpu => f.write_str("cpu"),
            Device::Gpu(n) => write!(f, "gpu:{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids() {
        assert_eq!("cpu".parse::<Device>().unwrap(), Device::Cpu);
        assert_eq!("GPU".parse::<Device>().unwrap(), Device::Gpu(0));
        assert_eq!("gpu:3".parse::<Device>().unwrap(), Device::Gpu(3));
        assert_eq!("cuda:1".parse::<Device>().unwrap(), Device::Gpu(1));
        assert!("gpu:16".parse::<Device>().is_err());
        assert!("tpu".parse::<Device>().is_err());
        assert_eq!(Device::Gpu(2).to_string(), "gpu:2");
    }
}
