//! Read-only IT8915 sensor access. No I2C writes are bound.
use crate::model::Reading;
use nvapi_sys::gpu::{NvAPI_EnumPhysicalGPUs, NvAPI_GPU_GetPCIIdentifiers};
use nvapi_sys::handles::NvPhysicalGpuHandle;
use nvapi_sys::i2c::private::{NV_I2C_INFO_EX_V3, NvAPI_I2CReadEx};
use nvapi_sys::i2c::{NV_I2C_INFO_VER3, NVAPI_I2C_SPEED_100KHZ, NVAPI_I2C_SPEED_DEPRECATED};
use nvapi_sys::nvapi::{NvAPI_Initialize, NvAPI_Unload};
use nvapi_sys::status::NVAPI_OK;
use nvapi_sys::types::NVAPI_MAX_PHYSICAL_GPUS;

pub struct AstralPowerMonitor {
    handle: NvPhysicalGpuHandle,
    pub name: &'static str,
}
pub fn model_for_id(id: u32) -> Option<&'static str> {
    Some(match id {
        0x89e31043 => "ROG Astral RTX 5090 OC",
        0x8a2e1043 => "ROG Astral RTX 5090 OC White",
        0x89ec1043 => "ROG Astral RTX 5090 LC",
        0x89ea1043 => "ROG Astral RTX 5090D OC",
        0x8a611043 => "ROG Matrix RTX 5090",
        0x89de1043 => "ROG Astral RTX 5080 OC",
        0x8a2b1043 => "ROG Astral RTX 5080 OC White",
        0x8a451043 => "ROG Astral RTX 5080 Hatsune Miku",
        0x89df1043 => "ROG Astral RTX 5080",
        // BTF slot-powered cards bypass the sensor: do not claim support.
        _ => return None,
    })
}
impl AstralPowerMonitor {
    pub fn new() -> Result<Self, String> {
        if std::mem::size_of::<NV_I2C_INFO_EX_V3>() != 64 {
            return Err("Unsupported sensor structure layout; x64 Windows is required".into());
        }
        let status = unsafe { NvAPI_Initialize() };
        if status != NVAPI_OK {
            return Err(format!("NVIDIA initialization failed: {status:?}"));
        }
        let result = Self::find_card();
        if result.is_err() {
            unsafe { NvAPI_Unload() };
        }
        result
    }
    fn find_card() -> Result<Self, String> {
        let mut handles = [unsafe { std::mem::zeroed() }; NVAPI_MAX_PHYSICAL_GPUS];
        let mut count = 0;
        let status = unsafe { NvAPI_EnumPhysicalGPUs(&mut handles, &mut count) };
        if status != NVAPI_OK || count as usize > handles.len() {
            return Err(format!("NVIDIA GPU enumeration failed: {status:?}"));
        }
        for handle in handles.into_iter().take(count as usize) {
            let (mut device, mut subsystem, mut revision, mut extended) = (0, 0, 0, 0);
            let status = unsafe {
                NvAPI_GPU_GetPCIIdentifiers(
                    handle,
                    &mut device,
                    &mut subsystem,
                    &mut revision,
                    &mut extended,
                )
            };
            if status == NVAPI_OK
                && let Some(name) = model_for_id(subsystem)
            {
                return Ok(Self { handle, name });
            }
        }
        Err("No supported ASUS Astral sensor-equipped GPU found".into())
    }
    pub fn read(&self) -> Result<Reading, String> {
        let mut data = [0u8; 24];
        let mut register = 0x80;
        let mut request = NV_I2C_INFO_EX_V3 {
            version: NV_I2C_INFO_VER3,
            displayMask: 0,
            bIsDDCPort: 0,
            i2cDevAddress: 0x2b << 1,
            pbI2cRegAddress: &mut register,
            regAddrSize: 1,
            pbData: data.as_mut_ptr(),
            pbRead: data.len() as u32,
            cbSize: NVAPI_I2C_SPEED_DEPRECATED,
            i2cSpeedKhz: NVAPI_I2C_SPEED_100KHZ,
            portId: 1,
            bIsPortIdSet: 1,
        };
        let mut read_status = 0;
        let status = unsafe { NvAPI_I2CReadEx(self.handle, &mut request, &mut read_status) };
        if status != NVAPI_OK {
            return Err(format!("Sensor read failed: {status:?}"));
        }
        Reading::decode(&data)
    }
}
impl Drop for AstralPowerMonitor {
    fn drop(&mut self) {
        unsafe { NvAPI_Unload() };
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selects_supported_card_and_rejects_unknown_or_btf() {
        assert_eq!(
            model_for_id(0x8a2e1043),
            Some("ROG Astral RTX 5090 OC White")
        );
        assert_eq!(model_for_id(0x12341043), None);
        assert_eq!(model_for_id(0x8a5a1043), None);
    }
    #[test]
    fn validates_request_layout() {
        assert_eq!(std::mem::size_of::<NV_I2C_INFO_EX_V3>(), 64);
    }
}
