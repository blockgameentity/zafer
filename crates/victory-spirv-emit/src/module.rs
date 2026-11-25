//! A SPIR-V module of one translated shader.
//!
//! SPIR-V has a rigid layout: capabilities, extensions, the memory model,
//! entry points, execution modes, debug names, decorations, **the types and
//! constants**, then the functions. A type declared after a function is
//! invalid. So every type this module can need is declared up front, and
//! the emitter below can request them in any order and get the declared id
//! back without a new instruction (the builder's caches do that).
//!
//! What is declared: `bool`, `i32`/`u32`, `f16`/`f32`/`f64`, each of those
//! as a 2-, 3- and 4-component vector, a pointer into every storage class a
//! register or interface variable lives in, and the interface itself.

use victory_spirv::builder::Builder;
use victory_spirv::enums::{BuiltIn, Decoration, StorageClass};
use victory_spirv::module::Id;
use victory_spirv::ops::{AddressingModel, Capability, ExecutionModel, MemoryModel};

/// Which stage a module is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Pixel,
    Compute,
}

impl Stage {
    /// The SPIR-V execution model.
    pub fn model(self) -> ExecutionModel {
        match self {
            Stage::Vertex => ExecutionModel::Vertex,
            Stage::Pixel => ExecutionModel::Fragment,
            Stage::Compute => ExecutionModel::GlCompute,
        }
    }
}

/// One interface variable: a location or a built-in, in or out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variable {
    /// The variable, as `OpVariable` declared it.
    pub id: Id,
    /// The value's type (the pointed-to type).
    pub ty: Id,
    /// Where it lives.
    pub space: StorageClass,
}

/// A shader module under construction: its declared types, its interface
/// and the variables the emitter loads and stores.
pub struct Module {
    builder: Builder,
    stage: Stage,
    /// `bool`.
    pub bool_ty: Id,
    /// `i32` (a register dword, and every scalar value).
    pub i32_ty: Id,
    /// `u32` (indices, lane ids, bit patterns read as unsigned).
    pub u32_ty: Id,
    /// `f32`.
    pub f32_ty: Id,
    /// `vec2<f32>` … `vec4<f32>`.
    pub f32_vecs: [Id; 3],
    /// `vec2<i32>` … `vec4<i32>`.
    pub i32_vecs: [Id; 3],
    /// `vec2<bool>` … `vec4<bool>`.
    pub bool_vecs: [Id; 3],
    /// The interface variables, in declaration order.
    interface: Vec<Id>,
    /// The type of the entry point's parameter list: none.
    void: Id,
    /// The `GLSL.std.450` set imported at construction.
    glsl: Id,
    /// The compute workgroup size, once the emitter knows the console's
    /// thread group (see [`Module::set_local_size`]).
    local_size: Option<[u32; 3]>,
}

impl Module {
    /// A module for `stage`, with every type it can need declared.
    pub fn new(stage: Stage) -> Module {
        let mut builder = Builder::new();
        builder.capability(Capability::Shader);
        // The console's addresses are integers the title computes, and a
        // shader dereferences them: that is a buffer device address, which
        // SPIR-V spells with the physical storage buffer capability and the
        // physical addressing model. `docs/vulkan-backend.md` picks it
        // because it needs no descriptor set per resource; the host only
        // has to enable `VK_EXT_buffer_device_address`.
        builder.capability(Capability::PhysicalStorageBufferAddresses);
        // The addressing model is the physical one; the memory model stays
        // GLSL 450's, because `VulkanMemoryModel` needs SPIR-V 1.5 and
        // Vulkan 1.2 ("not allowed by Vulkan 1.1"). Physical addressing does
        // not require it.
        builder.extension("SPV_EXT_physical_storage_buffer");
        builder.memory_model(AddressingModel::PhysicalStorageBuffer64, MemoryModel::Glsl450);
        // The extended instruction set a translated shader needs for what
        // SPIR-V core has no opcode for: `min`/`max`/`clamp` on integers,
        // `floor`/`ceil`/`round`/`trunc`/`fract`, `sin`/`cos`/`exp`/`log`/
        // `sqrt`/`inverse sqrt`, `length`, and the packed 16-bit
        // conversions `f16` requires.
        let glsl = builder.ext_inst_import("GLSL.std.450");
        let bool_ty = builder.bool();
        let i32_ty = builder.int(32, true);
        let u32_ty = builder.int(32, false);
        let f32_ty = builder.float(32);
        let f32_vecs = [
            builder.vector(f32_ty, 2),
            builder.vector(f32_ty, 3),
            builder.vector(f32_ty, 4),
        ];
        let i32_vecs = [
            builder.vector(i32_ty, 2),
            builder.vector(i32_ty, 3),
            builder.vector(i32_ty, 4),
        ];
        let bool_vecs = [
            builder.vector(bool_ty, 2),
            builder.vector(bool_ty, 3),
            builder.vector(bool_ty, 4),
        ];
        let void = builder.void();
        Module {
            builder,
            stage,
            bool_ty,
            i32_ty,
            u32_ty,
            f32_ty,
            f32_vecs,
            i32_vecs,
            bool_vecs,
            glsl,
            local_size: None,
            interface: Vec::new(),
            void,
        }
    }

    /// The console's thread group: the workgroup a compute shader runs in.
    ///
    /// Vulkan requires one declared for a `GLCompute` entry point, and it
    /// has to be the console's, because a program's thread id, its lane
    /// masks and its shared memory are all laid out in a group of that
    /// shape. The mode itself is written in [`Module::finish`], which is
    /// where the entry point's id exists.
    pub fn set_local_size(&mut self, x: u32, y: u32, z: u32) {
        self.local_size = Some([x.max(1), y.max(1), z.max(1)]);
    }

    /// The `GLSL.std.450` set's id, for `OpExtInst`.
    pub fn glsl(&self) -> Id {
        self.glsl
    }

    /// The builder, for the emitter's instructions.
    pub fn builder(&mut self) -> &mut Builder {
        &mut self.builder
    }

    /// The stage this module is for.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// `void`.
    pub fn void(&self) -> Id {
        self.void
    }

    /// The vector type of `n` (2-4) components of `scalar`: a float scalar
    /// gives `vecN<f32>`, an integer one `vecN<i32>`, a bool one
    /// `vecN<bool>`.
    pub fn vec(&self, scalar: Id, n: u32) -> Id {
        let set = if scalar == self.f32_ty {
            &self.f32_vecs
        } else if scalar == self.bool_ty {
            &self.bool_vecs
        } else {
            &self.i32_vecs
        };
        match n {
            2 => set[0],
            3 => set[1],
            4 => set[2],
            _ => unreachable!("a vector has 2, 3 or 4 components"),
        }
    }

    /// A vertex input at `location`, of `ty`.
    pub fn input_at(&mut self, location: u32, ty: Id) -> Variable {
        let id = self.builder.variable(ty, StorageClass::Input, None);
        self.builder.decorate_location(id, location);
        self.interface.push(id);
        Variable {
            id,
            ty,
            space: StorageClass::Input,
        }
    }

    /// A fragment output at `location`, of `ty`.
    pub fn output_at(&mut self, location: u32, ty: Id) -> Variable {
        let id = self.builder.variable(ty, StorageClass::Output, None);
        self.builder.decorate_location(id, location);
        self.interface.push(id);
        Variable {
            id,
            ty,
            space: StorageClass::Output,
        }
    }

    /// A built-in variable: `gl_Position`, `gl_FragCoord`, `gl_FragDepth`.
    pub fn builtin(&mut self, builtin: BuiltIn, ty: Id) -> Variable {
        let space = match builtin {
            BuiltIn::Position | BuiltIn::FragDepth | BuiltIn::PointSize => StorageClass::Output,
            _ => StorageClass::Input,
        };
        let id = self.builder.variable(ty, space, None);
        self.builder.decorate_builtin(id, builtin);
        // A depth output replaces the interpolated depth.
        if builtin == BuiltIn::FragDepth {
            self.builder
                .decorate(id, Decoration::NoPerspective, &[]);
        }
        self.interface.push(id);
        Variable { id, ty, space }
    }

    /// The parameter block: a storage buffer of dwords, one per root
    /// parameter.
    ///
    /// A root parameter's value on the console *is* the address of what it
    /// describes, so the block carries those addresses and the shader turns
    /// each into a physical pointer. It is a storage buffer rather than a
    /// push constant because Vulkan only guarantees 128 bytes of push
    /// constant, and the title's layouts are larger; a runtime array has no
    /// such limit and the host binds it once per pipeline.
    pub fn parameters(&mut self, count: u32) -> Variable {
        let array = self.builder.runtime_array(self.u32_ty);
        self.builder
            .decorate(array, Decoration::ArrayStride, &[4]);
        let block = self.builder.struct_of(&[array]);
        self.builder.decorate(block, Decoration::Block, &[]);
        self.builder.decorate_offset(block, 0, 0);
        let id = self.builder.variable(block, StorageClass::StorageBuffer, None);
        self.builder.decorate(id, Decoration::DescriptorSet, &[0]);
        self.builder.decorate(id, Decoration::Binding, &[0]);
        Variable {
            id,
            ty: block,
            space: StorageClass::StorageBuffer,
        }
    }

    /// A module-scope variable of `ty` in `space` (a register file, a user
    /// data buffer), with no initializer.
    pub fn global(&mut self, ty: Id, space: StorageClass) -> Variable {
        let id = self.builder.variable(ty, space, None);
        Variable { id, ty, space }
    }

    /// Finishes `body` into the module and returns the file.
    ///
    /// `body` receives the entry point's function and its entry block; it
    /// writes the shader's instructions and ends the function. The entry
    /// point and the interface are written around it. The module's sections
    /// are reordered by the writer, so the order these are emitted in does
    /// not matter.
    pub fn finish<F>(mut self, name: &str, body: F) -> Vec<u8>
    where
        F: FnOnce(&mut Builder, victory_spirv::builder::Function),
    {
        let function = self
            .builder
            .function(name, self.void, &[], victory_spirv::enums::function_control::NONE);
        // A fragment shader must say where its origin is; the console's
        // pixel coordinate origin is the upper left, as the hardware's is.
        if self.stage == Stage::Pixel {
            self.builder.execution_mode(
                function.id,
                victory_spirv::ops::ExecutionMode::OriginUpperLeft,
                &[],
            );
        }
        // A compute shader must declare its workgroup size: Vulkan has no
        // default, and `spirv-val` rejects a `GLCompute` entry point
        // without `LocalSize`. The emitter sets the real one from the
        // console's thread group before it finishes (`set_local_size`); this
        // is the single-threaded fallback.
        if self.stage == Stage::Compute {
            let [x, y, z] = self.local_size.unwrap_or([1, 1, 1]);
            self.builder.execution_mode(
                function.id,
                victory_spirv::ops::ExecutionMode::LocalSize,
                &[x, y, z],
            );
        }
        body(&mut self.builder, function);
        let interface = self.interface.clone();
        self.builder
            .entry_point(self.stage.model(), function.id, name, &interface);
        self.builder.finish()
    }
}
