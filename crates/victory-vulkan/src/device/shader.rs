//! Turning an emitted SPIR-V module into something Vulkan will run.
//!
//! The emitter's output is a `Vec<u8>` of words; `vkCreateShaderModule`
//! wants exactly that, so the only work here is the ownership and the check
//! that the bytes really are a module (a truncated or empty buffer produces
//! a driver error far away from its cause). The stage is *not* part of a
//! module: it is given at pipeline creation, so nothing here guesses it.

use ash::vk;

use super::create::Device;

/// A `VkShaderModule` that destroys itself with the device.
pub struct ShaderModule {
    device: ash::Device,
    handle: vk::ShaderModule,
}

impl Device {
    /// Creates a shader module from an emitted SPIR-V module.
    ///
    /// Fails before touching the driver when `spirv` is empty or not a
    /// whole number of words, because both are mistakes in the emitter
    /// rather than something a driver can report usefully.
    pub fn shader_module(&self, spirv: &[u8]) -> Result<ShaderModule, String> {
        if spirv.is_empty() {
            return Err("an empty SPIR-V module".to_owned());
        }
        if !spirv.len().is_multiple_of(4) {
            return Err(format!(
                "a SPIR-V module must be a whole number of words, not {} bytes",
                spirv.len()
            ));
        }
        let words = words(spirv);
        let create = vk::ShaderModuleCreateInfo::default().code(&words);
        // SAFETY: `words` is a whole number of words, and `self` owns the
        // device it is created on.
        let handle = unsafe { self.device().create_shader_module(&create, None) }.map_err(|e| {
            tracing::error!(words = words.len(), "vkCreateShaderModule failed: {e}");
            format!("vkCreateShaderModule: {e}")
        })?;
        tracing::debug!(words = words.len(), "shader module created");
        Ok(ShaderModule {
            device: self.device().clone(),
            handle,
        })
    }
}

impl ShaderModule {
    /// The handle, for a pipeline's stage.
    pub fn handle(&self) -> vk::ShaderModule {
        self.handle
    }
}

impl Drop for ShaderModule {
    fn drop(&mut self) {
        // SAFETY: created by this object, and we hold the device clone that
        // created it, so the device cannot have been dropped first.
        unsafe { self.device.destroy_shader_module(self.handle, None) };
    }
}

/// The module's bytes as the `u32` words `vkCreateShaderModule` takes.
///
/// `spirv` is already little-endian (SPIR-V is defined that way), so this is
/// a reinterpretation, not a byte swap.
fn words(spirv: &[u8]) -> Vec<u32> {
    // `bytemuck` is the checked form of exactly this reinterpretation: it
    // verifies the length and `u32`'s alignment (guaranteed, since `spirv`
    // came from a `Vec<u32>`'s bytes), and returns a borrow rather than a
    // copy. The length check is done by the caller.
    bytemuck::cast_slice(spirv).to_vec()
}
