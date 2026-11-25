//! Instructions whose meaning is their immediate: `s_nop`, the wait counts,
//! the cache controls and the program end.
//!
//! Scheduling-only instructions have no SPIR-V form because SPIR-V has no
//! schedule; they are dropped. `s_endpgm` ends the program, which for the
//! room a block has is a return.

use victory_shader::ir::Inst;
use victory_spirv::builder::Builder;

use super::Emitter;
use crate::Error;

/// The `s_sendmsg` id of a mesh or NGG stage's output allocation request.
const GS_ALLOC_REQ: u16 = 9;

impl<'a> Emitter<'a> {
    /// One immediate-only instruction.
    pub(crate) fn immediate(&mut self, b: &mut Builder, inst: &Inst, imm: u16) -> Result<(), Error> {
        match inst.name() {
            // The schedule: SPIR-V has none, so these have no form.
            "s_nop" | "s_waitcnt" | "s_waitcnt_depctr" | "s_waitcnt_vscnt" | "s_wait_loadcnt"
            | "s_wait_storecnt" | "s_wait_samplecnt" | "s_wait_bvhcnt" | "s_wait_kmcnt"
            | "s_wait_dscnt" | "s_wait_expcnt" | "s_wait_xcnt" | "s_inst_prefetch"
            | "s_sleep" | "s_setprio" | "s_sched_barrier" => Ok(()),
            // The output allocation request of a merged stage: the host's
            // rasteriser allocates, so nothing is asked for.
            "s_sendmsg" if imm & 0xF == GS_ALLOC_REQ => Ok(()),
            "s_endpgm" => {
                b.return_void();
                Ok(())
            }
            "s_icache_inv" | "s_dcache_inv" | "s_dcache_inv_vol" => Ok(()),
            _ => Err(super::unsupported(inst)),
        }
    }
}
