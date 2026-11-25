use super::*;

impl Instruction {
    pub(crate) fn valu(&self, name: &str, lanes: u8) -> Valu {
        // No operands; the console compiler stores a shader hash in their fields.
        if matches!(name, "v_nop" | "v_pipeflush" | "v_clrexcp") {
            return Valu::default();
        }
        let w = |i: usize| self.words.get(i).copied().unwrap_or(0);
        let w0 = w(0);
        let (dst_n, src_n) = valu_dwords(name);
        let compare = match self.encoding {
            Encoding::Vopc => true,
            Encoding::Vop3 => self.opcode < 0x100,
            _ => false,
        };
        let vop3b = VOP3B.contains(&name);
        let carry_in = name.ends_with("_co_ci_u32") || name == "v_cndmask_b32";
        let mut v = Valu::default();
        match self.encoding {
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc => {
                let ext = w(1);
                let mut src0 = w0 & 0x1FF;
                let vsrc1 = 256 + (w0 >> 9 & 0xFF);
                let mut src1 = vsrc1;
                match src0 {
                    0xF9 => {
                        let sdwa = Sdwa {
                            dst_sel: (ext >> 8 & 7) as u8,
                            dst_unused: (ext >> 11 & 3) as u8,
                            src_sel: [(ext >> 16 & 7) as u8, (ext >> 24 & 7) as u8],
                            sext: (ext >> 19 & 1 | ext >> 26 & 2) as u8,
                        };
                        // The s0/s1 bits select an SGPR or constant instead of a VGPR.
                        src0 = (ext & 0xFF) | if ext >> 23 & 1 == 0 { 256 } else { 0 };
                        if ext >> 31 == 1 {
                            src1 = vsrc1 - 256;
                        }
                        v.neg = (ext >> 20 & 1 | ext >> 27 & 2) as u8;
                        v.abs = (ext >> 21 & 1 | ext >> 28 & 2) as u8;
                        if compare {
                            if ext >> 15 & 1 == 1 {
                                v.dst = Some(scalar((ext >> 8 & 0x7F) as u8, lanes));
                            }
                            v.sdwa = Some(Sdwa {
                                dst_sel: 6,
                                dst_unused: 0,
                                ..sdwa
                            });
                        } else {
                            v.clamp = ext >> 13 & 1 == 1;
                            v.omod = (ext >> 14 & 3) as u8;
                            v.sdwa = Some(sdwa);
                        }
                    }
                    0xFA => {
                        src0 = 256 + (ext & 0xFF);
                        v.neg = (ext >> 20 & 1 | ext >> 21 & 2) as u8;
                        v.abs = (ext >> 21 & 1 | ext >> 22 & 2) as u8;
                        v.dpp = Some(Dpp::Row {
                            ctrl: (ext >> 8 & 0x1FF) as u16,
                            row_mask: (ext >> 28) as u8,
                            bank_mask: (ext >> 24 & 0xF) as u8,
                            bound_ctrl: ext >> 19 & 1 == 1,
                            fi: ext >> 18 & 1 == 1,
                        });
                    }
                    0xE9 | 0xEA => {
                        v.dpp = Some(Dpp::Lanes {
                            sel: ext >> 8,
                            fi: src0 == 0xEA,
                        });
                        src0 = 256 + (ext & 0xFF);
                    }
                    _ => {}
                }
                let op0 = source(src0, src_n[0], ext);
                let op1 = source(src1, src_n[1], ext);
                match self.encoding {
                    Encoding::Vopc => {
                        let mask = if name.starts_with("v_cmpx") {
                            EXEC
                        } else {
                            VCC
                        };
                        v.dst = v.dst.or(Some(scalar(mask, lanes)));
                        v.src = vec![op0, op1];
                    }
                    Encoding::Vop1 => {
                        v.dst = Some(valu_dst(name, w0 >> 17 & 0xFF, dst_n));
                        v.src = vec![op0];
                    }
                    _ => {
                        v.dst = Some(vector(w0 >> 17 & 0xFF, dst_n));
                        v.src = match name {
                            n if n.ends_with("mk_f32") || n.ends_with("mk_f16") => {
                                vec![op0, Operand::Literal(ext), op1]
                            }
                            n if n.ends_with("ak_f32") || n.ends_with("ak_f16") => {
                                vec![op0, op1, Operand::Literal(ext)]
                            }
                            _ => vec![op0, op1],
                        };
                        if carry_in {
                            v.src.push(scalar(VCC, lanes));
                        }
                        if name.ends_with("_co_ci_u32") {
                            v.carry = Some(scalar(VCC, lanes));
                        }
                    }
                }
            }
            Encoding::Vop3 | Encoding::Vop3p => {
                let w1 = w(1);
                let field = |i: u32| w1 >> (9 * i) & 0x1FF;
                let count = match self.opcode {
                    _ if self.encoding == Encoding::Vop3p => {
                        if name.starts_with("v_pk_")
                            && !name.contains("_fma_")
                            && !name.contains("_mad_")
                        {
                            2
                        } else {
                            3
                        }
                    }
                    0x000..0x100 => 2,
                    0x100..0x140 => 2 + usize::from(carry_in),
                    0x180..0x200 => 1,
                    _ if VOP3_TWO_SOURCES.contains(&name) => 2,
                    _ => 3,
                };
                v.src = (0..count)
                    .map(|i| {
                        let code = field(i as u32);
                        if i == 0 && name.starts_with("v_interp_") {
                            Operand::Attr {
                                index: (code & 0x3F) as u8,
                                chan: (code >> 6 & 3) as u8,
                                high: code >> 8 & 1 == 1,
                            }
                        } else if i == 2 && carry_in {
                            source(code, lanes, w(2))
                        } else {
                            source(code, src_n[i], w(2))
                        }
                    })
                    .collect();
                v.clamp = w0 >> 15 & 1 == 1;
                v.neg = (w1 >> 29) as u8;
                if self.encoding == Encoding::Vop3p {
                    v.neg_hi = (w0 >> 8 & 7) as u8;
                    v.opsel = (w0 >> 11 & 7) as u8;
                    v.opsel_hi = (w1 >> 27 & 3 | w0 >> 12 & 4) as u8;
                } else {
                    v.omod = (w1 >> 27 & 3) as u8;
                    if vop3b {
                        v.carry = Some(scalar((w0 >> 8 & 0x7F) as u8, lanes));
                    } else {
                        v.abs = (w0 >> 8 & 7) as u8;
                        v.opsel = (w0 >> 11 & 0xF) as u8;
                    }
                }
                let vdst = w0 & 0xFF;
                v.dst = Some(if compare {
                    scalar(vdst as u8, lanes)
                } else {
                    valu_dst(name, vdst, dst_n)
                });
            }
            _ => unreachable!("not a VALU encoding"),
        }
        v
    }
}
