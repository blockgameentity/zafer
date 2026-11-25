//! Memory: the console's scalar, global, buffer and image accesses.
//!
//! A console address is a host address, and memory is unified: the title
//! writes a resource and the shader reads the same bytes. Vulkan says that
//! with `PhysicalStorageBuffer` — a buffer device address — so an access is
//! an `OpLoad`/`OpStore` through the address the shader computes, and a root
//! parameter's value *is* that address, handed over as a push constant. That
//! is the mapping `docs/vulkan-backend.md` gives.
//!
//! The address arithmetic mirrors the DXIL emitter's (`emit/lower.rs`,
//! `flat`/`smem`): a [`Address`] is a root parameter's value plus a constant
//! and at most one computed term, so the pointer is the push constant's
//! dword converted with `OpConvertUToPtr`, then `OpAccessChain`ed by the
//! byte offset. Shared memory is the odd one out: it is per workgroup, so it
//! is a `Workgroup` variable indexed by dword.

use victory_shader::gfx10::{Fields, Operand};
use victory_shader::ir::{Inst, Reg, Value};
use victory_shader::resource::{Address, Site, Source, CODE};
use victory_spirv::builder::Builder;
use victory_spirv::enums::StorageClass;
use victory_spirv::module::Id;

use super::{unsupported, Emitter};
use crate::Error;

/// The bytes in one dword, the unit both offsets and indices count in.
const DWORD: u32 = 4;

/// Dwords a push constant block may hold: Vulkan's guaranteed 128 bytes.
pub(crate) const PUSH_CONSTANT_DWORDS: u32 = 32;

impl<'a> Emitter<'a> {
    /// One memory instruction, dispatched by its encoding.
    pub(crate) fn memory(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        match &inst.fields {
            Fields::Smem { .. } => self.smem(b, inst),
            Fields::Flat { .. } => self.flat(b, inst),
            Fields::Ds { .. } => self.ds(b, inst),
            // A descriptor-built buffer (MUBUF/MTBUF) names its resource by
            // four dwords the program assembles; resolving which descriptor
            // that is belongs to the tracer (`Resources::descriptors`), and
            // until the emitter reads those it must refuse rather than
            // guess an address.
            Fields::Buffer { .. } | Fields::Image { .. } => Err(unsupported(inst)),
            _ => Err(unsupported(inst)),
        }
    }

    /// A scalar load: `s_load_dword*` through a root parameter.
    ///
    /// Every dword of the result is one `OpLoad` of one `u32`, in register
    /// order, so the arm does not depend on the shape of the load.
    fn smem(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let Fields::Smem { data, .. } = &inst.fields else {
            return Err(unsupported(inst));
        };
        let Some(Operand::Scalar { code, .. }) = *data else {
            return Err(unsupported(inst));
        };
        let n = dwords(inst.name()).ok_or_else(|| unsupported(inst))?;
        let address = self.address(inst)?;
        let base = self.base_pointer(b, address)?;
        let offset = self.byte_offset(b, address)?;
        for k in 0..n {
            let ptr = self.dword_pointer(b, base, offset, k);
            let value = b.load(self.types.u32_, ptr);
            let bits = b.bitcast(self.types.i32_, value);
            self.store_i32(b, Reg::S(code.wrapping_add(k)), bits);
        }
        Ok(())
    }

    /// A global (FLAT/GLOBAL) load or store, each lane at its own address:
    /// `global_load_dword*` and its stores.
    fn flat(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let Fields::Flat { dst, data, .. } = &inst.fields else {
            return Err(unsupported(inst));
        };
        let name = inst.name();
        let storing = name.starts_with("global_store_") || name.starts_with("flat_store_");
        let n = dwords(name).ok_or_else(|| unsupported(inst))?;
        let address = self.address(inst)?;
        let base = self.base_pointer(b, address)?;
        let offset = self.byte_offset(b, address)?;
        for k in 0..n {
            let ptr = self.dword_pointer(b, base, offset, k);
            if storing {
                let Some(Operand::Vector { reg, .. }) = *data else {
                    return Err(unsupported(inst));
                };
                let value = self.load_i32(b, self.read_value(inst, Reg::V(reg.wrapping_add(k))));
                let Some(value) = value else {
                    return Err(unsupported(inst));
                };
                let bits = b.bitcast(self.types.u32_, value);
                b.store(ptr, bits);
            } else {
                let Some(Operand::Vector { reg, .. }) = *dst else {
                    return Err(unsupported(inst));
                };
                let value = b.load(self.types.u32_, ptr);
                let bits = b.bitcast(self.types.i32_, value);
                self.store_i32(b, Reg::V(reg.wrapping_add(k)), bits);
            }
        }
        Ok(())
    }

    /// LDS: the workgroup's shared memory, one `Workgroup` variable.
    ///
    /// The console's LDS is a byte-addressed scratch space per workgroup.
    /// Vulkan's `Workgroup` storage class is exactly that, so the variable is
    /// a runtime array of dwords and an access indexes it by the address's
    /// dword index.
    fn ds(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let Fields::Ds {
            dst,
            addr,
            data,
            offset0,
            ..
        } = &inst.fields
        else {
            return Err(unsupported(inst));
        };
        let name = inst.name();
        let storing = name.starts_with("ds_write") || name.starts_with("ds_add") || name.starts_with("ds_sub");
        let n = dwords(name).ok_or_else(|| unsupported(inst))?;
        let shared = self.shared_memory(b);
        // The address is a dword offset: `offset0` plus what the lane
        // computed.
        let mut at = b.constant_i32(i32::from(*offset0));
        if let Some(Operand::Vector { reg, .. }) = *addr {
            let value = self.load_i32(b, self.read_value(inst, Reg::V(reg)));
            let Some(value) = value else {
                return Err(unsupported(inst));
            };
            at = b.iadd(self.types.i32_, value, at);
        }
        let at = b.uconvert(self.types.u32_, at);
        let ptr_ty = b.pointer(StorageClass::Workgroup, self.types.u32_);
        for k in 0..n {
            let index = if k == 0 {
                at
            } else {
                let step = b.constant_u32(u32::from(k));
                b.iadd(self.types.u32_, at, step)
            };
            let ptr = b.access_chain(ptr_ty, shared, &[index]);
            if storing {
                let Some(Operand::Vector { reg, .. }) = data.first().copied() else {
                    return Err(unsupported(inst));
                };
                let value = self.load_i32(b, self.read_value(inst, Reg::V(reg.wrapping_add(k))));
                let Some(value) = value else {
                    return Err(unsupported(inst));
                };
                let bits = b.bitcast(self.types.u32_, value);
                b.store(ptr, bits);
            } else {
                let Some(Operand::Vector { reg, .. }) = *dst else {
                    return Err(unsupported(inst));
                };
                let value = b.load(self.types.u32_, ptr);
                let bits = b.bitcast(self.types.i32_, value);
                self.store_i32(b, Reg::V(reg.wrapping_add(k)), bits);
            }
        }
        Ok(())
    }

    /// The address the tracer found for the instruction being emitted.
    fn address(&self, inst: &Inst) -> Result<Address, Error> {
        let site = self.site;
        if let Some((_, address)) = self.res.scalar.iter().find(|(at, _)| *at == site) {
            return Ok(*address);
        }
        // A buffer load names its resource by a descriptor, not by an
        // address: `s_buffer_*` is a typed view of a root parameter's buffer
        // rather than a pointer the shader dereferences, so its address is
        // the descriptor's own. The tracer keeps those with the descriptors.
        if let Some(d) = self.res.descriptors.iter().find(|d| d.site == site) {
            if let Some(address) = first_address(&d.source, &self.res.scalar) {
                return Ok(address);
            }
        }
        Err(Error::Unsupported {
            offset: inst.machine.offset,
            what: "an access with no address the tracer found",
        })
    }

    /// The physical pointer a root parameter's address starts at.
    fn base_pointer(&mut self, b: &mut Builder, address: Address) -> Result<Id, Error> {
        let ptr_ty = b.pointer(StorageClass::PhysicalStorageBuffer, self.types.u32_);
        if address.param == CODE {
            return Err(Error::Unsupported {
                offset: 0,
                what: "the program's own constant table",
            });
        }
        let slot = self.parameter_value(b, address.param)?;
        Ok(b.convert_u_to_ptr(ptr_ty, slot))
    }

    /// The byte offset of `address` within its parameter, as a `u32`.
    fn byte_offset(&mut self, b: &mut Builder, address: Address) -> Result<Id, Error> {
        let base = b.constant_u32(address.offset);
        match address.dynamic {
            None => Ok(base),
            Some(d) => {
                let Some(value) = self.load_i32(b, d) else {
                    return Err(Error::Unsupported {
                        offset: 0,
                        what: "an untracked dynamic address term",
                    });
                };
                let bits = b.bitcast(self.types.u32_, value);
                Ok(b.iadd(self.types.u32_, base, bits))
            }
        }
    }

    /// The pointer to dword `k` of the buffer `base` at `offset`.
    fn dword_pointer(&self, b: &mut Builder, base: Id, offset: Id, k: u8) -> Id {
        let ptr_ty = b.pointer(StorageClass::PhysicalStorageBuffer, self.types.u32_);
        let at = if k == 0 {
            offset
        } else {
            let step = b.constant_u32(u32::from(k) * DWORD);
            b.iadd(self.types.u32_, offset, step)
        };
        // The console's address is a byte address, so the byte offset is the
        // index into a byte array; a `u32` element is what is read or
        // written, so the index is the byte offset over four.
        let four = b.constant_u32(DWORD);
        let index = b.udiv(self.types.u32_, at, four);
        b.access_chain(ptr_ty, base, &[index])
    }

    /// The push constant dword that holds root parameter `param`'s value.
    fn parameter_value(&mut self, b: &mut Builder, param: usize) -> Result<Id, Error> {
        let Some(block) = self.parameters else {
            return Err(Error::Unsupported {
                offset: 0,
                what: "a root parameter with no parameter block",
            });
        };
        let Ok(slot) = u32::try_from(param) else {
            return Err(Error::Unsupported {
                offset: 0,
                what: "a root parameter beyond the parameter block",
            });
        };
        // `block` is `struct { u32[] }`, so the chain is member 0, then the
        // element.
        let index = b.constant_u32(slot);
        let u32_ = self.types.u32_;
        let ptr_ty = b.pointer(StorageClass::StorageBuffer, u32_);
        let array = b.runtime_array(u32_);
        let array_ty = b.pointer(StorageClass::StorageBuffer, array);
        let member = b.constant_u32(0);
        let array = b.access_chain(array_ty, block, &[member]);
        let at = b.access_chain(ptr_ty, array, &[index]);
        Ok(b.load(self.types.u32_, at))
    }

    /// The workgroup's shared memory, made once.
    fn shared_memory(&mut self, b: &mut Builder) -> Id {
        if let Some(id) = self.shared {
            return id;
        }
        let array = b.runtime_array(self.types.u32_);
        let id = b.variable(array, StorageClass::Workgroup, None);
        self.shared = Some(id);
        id
    }
}

/// The address a descriptor's source is built around, when it has one.
///
/// A descriptor is four dwords: a base address, a stride, and the size. A
/// source is a resource only when its base address is a root parameter's:
/// `Source::Built` carries one directly, and `Source::Loaded` names the
/// scalar load whose own address is the parameter (a descriptor read out of
/// a constant buffer). Anything assembled from arbitrary dwords (`Constant`,
/// a per-lane fetch) does not name memory the shader can reach, and the
/// access must then be refused rather than given a guessed pointer.
fn first_address(source: &Source, scalar: &[(Site, Address)]) -> Option<Address> {
    source.leaves().into_iter().find_map(|leaf| match leaf {
        Source::Built { address, .. } => Some(*address),
        Source::Loaded { load, .. } => scalar
            .iter()
            .find(|(at, _)| at == load)
            .map(|(_, address)| *address),
        _ => None,
    })
}

/// The dwords a load or store reads or writes, from its mnemonic.
///
/// The suffix is the hardware's own vocabulary (`dword`, `dwordx4`,
/// `ubyte`, `short`), so the count is a property of the name and not of the
/// encoding.
fn dwords(name: &str) -> Option<u8> {
    let rest = name
        .strip_prefix("s_load_")
        .or_else(|| name.strip_prefix("s_buffer_load_"))
        .or_else(|| name.strip_prefix("s_store_"))
        .or_else(|| name.strip_prefix("s_buffer_store_"))
        .or_else(|| name.strip_prefix("global_load_"))
        .or_else(|| name.strip_prefix("global_store_"))
        .or_else(|| name.strip_prefix("flat_load_"))
        .or_else(|| name.strip_prefix("flat_store_"))
        .or_else(|| name.strip_prefix("ds_read_"))
        .or_else(|| name.strip_prefix("ds_write_"))?;
    Some(match rest {
        "dword" | "ubyte" | "sbyte" | "ushort" | "sshort" | "u8" | "i8" | "b32" => 1,
        "dwordx2" | "b64" => 2,
        "dwordx3" | "b96" => 3,
        "dwordx4" | "b128" => 4,
        "dwordx8" | "b256" => 8,
        "dwordx16" | "b512" => 16,
        _ => return None,
    })
}

/// The value `inst` reads from `reg`.
trait ReadValue {
    fn read_value(&self, inst: &Inst, reg: Reg) -> Value;
}

impl ReadValue for Emitter<'_> {
    fn read_value(&self, inst: &Inst, reg: Reg) -> Value {
        inst.read(reg).unwrap_or(Value(u32::MAX))
    }
}
