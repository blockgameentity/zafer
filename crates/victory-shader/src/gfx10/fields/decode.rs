use super::*;

impl Instruction {
    /// Decodes the operand fields. `wave64` sizes lane masks.
    pub fn fields(&self, wave64: bool) -> Fields {
        let name = self.name().unwrap_or("");
        let w = |i: usize| self.words.get(i).copied().unwrap_or(0);
        let (w0, w1) = (w(0), w(1));
        let bit = |word: u32, n: u32| word >> n & 1 != 0;
        let cache = |glc, slc, dlc| Cache { glc, slc, dlc };
        match self.encoding {
            Encoding::Sop2 | Encoding::Sop1 | Encoding::Sopc => {
                let (dst, src0, src1) = salu_dwords(name);
                let sdst = scalar((w0 >> 16 & 0x7F) as u8, dst);
                let ssrc = |shift: u32, dwords| source(w0 >> shift & 0xFF, dwords, w1);
                let (dst, src) = match self.encoding {
                    Encoding::Sop2 => (Some(sdst), vec![ssrc(0, src0), ssrc(8, src1)]),
                    Encoding::Sopc => (None, vec![ssrc(0, src0), ssrc(8, src1)]),
                    _ => match name {
                        "s_getpc_b64" => (Some(sdst), vec![]),
                        "s_setpc_b64" | "s_rfe_b64" => (None, vec![ssrc(0, src0)]),
                        _ => (Some(sdst), vec![ssrc(0, src0)]),
                    },
                };
                Fields::Sop {
                    dst,
                    src,
                    imm: None,
                }
            }
            Encoding::Sopk => {
                let sdst = scalar((w0 >> 16 & 0x7F) as u8, 1 + u8::from(name == "s_call_b64"));
                let (dst, src) = match name {
                    "s_setreg_imm32_b32" => (None, vec![Operand::Literal(w1)]),
                    "s_version" => (None, vec![]),
                    n if n.starts_with("s_cmpk_")
                        || n.starts_with("s_waitcnt_")
                        || n == "s_setreg_b32" =>
                    {
                        (None, vec![sdst])
                    }
                    _ => (Some(sdst), vec![]),
                };
                Fields::Sop {
                    dst,
                    src,
                    imm: Some(w0 as u16),
                }
            }
            Encoding::Sopp => Fields::Sopp { imm: w0 as u16 },
            Encoding::Smem => {
                let data = match name {
                    "s_gl1_inv" | "s_dcache_inv" | "s_dcache_wb" => None,
                    n if n.starts_with("s_dcache_discard") => None,
                    "s_memtime" | "s_memrealtime" => Some(2),
                    _ => Some(data_dwords(name)),
                };
                let base = 2 + 2 * u8::from(name.starts_with("s_buffer_"));
                Fields::Smem {
                    data: data.map(|d| scalar((w0 >> 6 & 0x7F) as u8, d)),
                    base: scalar(((w0 & 0x3F) << 1) as u8, base),
                    offset: (w1 << 11) as i32 >> 11,
                    soffset: scalar((w1 >> 25) as u8, 1),
                    cache: cache(bit(w0, 16), false, bit(w0, 14)),
                }
            }
            Encoding::Vop1 | Encoding::Vop2 | Encoding::Vopc | Encoding::Vop3 | Encoding::Vop3p => {
                Fields::Valu(self.valu(name, if wave64 { 2 } else { 1 }))
            }
            Encoding::Vintrp => {
                let vsrc = (w0 & 0xFF) as u8;
                Fields::Vintrp {
                    dst: vector(w0 >> 18, 1),
                    src: if name == "v_interp_mov_f32" {
                        Operand::Const(vsrc)
                    } else {
                        vector(vsrc.into(), 1)
                    },
                    attr: Operand::Attr {
                        index: (w0 >> 10 & 0x3F) as u8,
                        chan: (w0 >> 8 & 3) as u8,
                        high: false,
                    },
                }
            }
            Encoding::Ds => {
                let ty = data_dwords(name);
                let pair = ds_pair(name);
                let returns = name.contains("read")
                    || name.contains("_rtn")
                    || matches!(
                        name,
                        "ds_swizzle_b32"
                            | "ds_permute_b32"
                            | "ds_bpermute_b32"
                            | "ds_append"
                            | "ds_consume"
                            | "ds_ordered_count"
                    );
                let no_addr = name.starts_with("ds_gws")
                    || name.contains("addtid")
                    || matches!(name, "ds_nop" | "ds_append" | "ds_consume");
                let data = if name.contains("read")
                    || name.contains("src2")
                    || matches!(
                        name,
                        "ds_swizzle_b32"
                            | "ds_nop"
                            | "ds_append"
                            | "ds_consume"
                            | "ds_ordered_count"
                            | "ds_gws_sema_v"
                            | "ds_gws_sema_p"
                            | "ds_gws_sema_release_all"
                    ) {
                    0
                } else if pair || name.contains("cmpst") || name.contains("mskor") {
                    2
                } else {
                    1
                };
                Fields::Ds {
                    dst: returns.then(|| vector(w1 >> 24, ty * (1 + u8::from(pair)))),
                    addr: (!no_addr).then(|| vector(w1 & 0xFF, 1)),
                    data: (0..data)
                        .map(|i| vector(w1 >> (8 + 8 * i) & 0xFF, ty))
                        .collect(),
                    offset0: w0 as u8,
                    offset1: (w0 >> 8) as u8,
                    gds: bit(w0, 17),
                }
            }
            Encoding::Mubuf | Encoding::Mtbuf => {
                let (offen, idxen, tfe) = (bit(w0, 12), bit(w0, 13), bit(w1, 23));
                let mtbuf = self.encoding == Encoding::Mtbuf;
                Fields::Buffer {
                    data: (!name.ends_with("_inv"))
                        .then(|| vector(w1 >> 8 & 0xFF, data_dwords(name) + u8::from(tfe))),
                    addr: vector(w1 & 0xFF, 1 + u8::from(offen && idxen)),
                    rsrc: scalar(((w1 >> 16 & 0x1F) << 2) as u8, 4),
                    soffset: source(w1 >> 24, 1, 0),
                    offset: (w0 & 0xFFF) as u16,
                    offen,
                    idxen,
                    format: mtbuf.then_some((w0 >> 19 & 0x7F) as u8),
                    lds: !mtbuf && bit(w0, 16),
                    tfe,
                    cache: cache(bit(w0, 14), bit(w1, 22), bit(w0, 15)),
                }
            }
            Encoding::Mimg => {
                let (dim, dmask, a16, d16) = (
                    (w0 >> 3 & 7) as u8,
                    (w0 >> 8 & 0xF) as u8,
                    bit(w1, 30),
                    bit(w1, 31),
                );
                let (r128, tfe, lwe) = (bit(w0, 15), bit(w0, 16), bit(w0, 17));
                let mut data = if name.starts_with("image_gather4") {
                    4
                } else {
                    dmask.count_ones().max(1) as u8
                };
                if d16 {
                    data = data.div_ceil(2);
                }
                let count = image_addr_dwords(name, dim, a16);
                let addr = if w0 >> 1 & 3 == 0 {
                    vec![vector(w1 & 0xFF, count)]
                } else {
                    // NSA: one VGPR per address, the first in dword 1, the rest
                    // in the extra dwords.
                    (0..usize::from(count))
                        .map(|i| match i {
                            0 => vector(w1 & 0xFF, 1),
                            i => vector(w(2 + (i - 1) / 4) >> (8 * ((i - 1) % 4)) & 0xFF, 1),
                        })
                        .collect()
                };
                let sampled = ["image_sample", "image_gather4", "image_get_lod"]
                    .iter()
                    .any(|p| name.starts_with(p));
                Fields::Image {
                    data: vector(w1 >> 8 & 0xFF, data + u8::from(tfe || lwe)),
                    addr,
                    rsrc: scalar(((w1 >> 16 & 0x1F) << 2) as u8, if r128 { 4 } else { 8 }),
                    sampler: sampled.then(|| scalar(((w1 >> 21 & 0x1F) << 2) as u8, 4)),
                    dmask,
                    dim,
                    unorm: bit(w0, 12),
                    r128,
                    a16,
                    d16,
                    tfe,
                    lwe,
                    cache: cache(bit(w0, 13), bit(w0, 25), bit(w0, 7)),
                }
            }
            Encoding::Flat | Encoding::Global | Encoding::Scratch => {
                let saddr = (w1 >> 16 & 0x7F) as u8;
                let glc = bit(w0, 16);
                let n = data_dwords(name);
                let (load, atomic) = (name.contains("_load_"), name.contains("_atomic_"));
                let addr = match self.encoding {
                    Encoding::Flat => Some(vector(w1 & 0xFF, 2)),
                    Encoding::Global => Some(vector(w1 & 0xFF, 1 + u8::from(saddr == NULL))),
                    // With an SGPR base, scratch takes no VGPR.
                    _ => (saddr == NULL).then(|| vector(w1 & 0xFF, 1)),
                };
                let ret = if atomic && name.contains("cmpswap") {
                    n / 2
                } else {
                    n
                };
                Fields::Flat {
                    dst: (load || atomic && glc).then(|| vector(w1 >> 24, ret)),
                    addr,
                    data: (!load).then(|| vector(w1 >> 8 & 0xFF, n)),
                    saddr: (self.encoding != Encoding::Flat)
                        .then(|| scalar(saddr, 1 + u8::from(self.encoding == Encoding::Global))),
                    offset: match self.encoding {
                        Encoding::Flat => (w0 & 0x7FF) as i16,
                        _ => ((w0 << 4) as i16) >> 4,
                    },
                    lds: bit(w0, 13),
                    cache: cache(glc, bit(w0, 17), bit(w0, 12)),
                }
            }
            Encoding::Exp => {
                let compr = bit(w0, 10);
                Fields::Exp {
                    target: (w0 >> 4 & 0x3F) as u8,
                    src: std::array::from_fn(|i| {
                        let slot = if compr { i / 2 } else { i };
                        bit(w0, i as u32).then(|| vector(w1 >> (8 * slot) & 0xFF, 1))
                    }),
                    compr,
                    done: bit(w0, 11),
                    vm: bit(w0, 12),
                }
            }
        }
    }
}
