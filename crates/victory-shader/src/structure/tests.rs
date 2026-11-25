    use super::*;
    use crate::ir;

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn build(words: &[u32], wave64: bool) -> Program {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        ir::build(&code, wave64, 256).unwrap()
    }

    fn guarded(s: &Structured) -> usize {
        s.guards.iter().flatten().filter(|g| g.is_some()).count()
    }

    #[test]
    fn divergent_if_becomes_a_lane_branch() {
        let p = build(
            &[
                0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
                0xBE80_3C6A, // s_and_saveexec_b32 s0, vcc_lo
                0xBF88_0001, // s_cbranch_execz -> 16
                0x7E02_0280, // v_mov_b32 v1, 0
                0x887E_007E, // 16: s_or_b32 exec_lo, exec_lo, s0
                0x7E04_0280, // v_mov_b32 v2, 0
                S_ENDPGM,
            ],
            false,
        );
        let s = structure(&p).unwrap();
        let LaneExit::Branch {
            when,
            taken: 2,
            not_taken: 1,
        } = s.exits[0]
        else {
            panic!("{:?}", s.exits)
        };
        // Skipped exactly where the compare fails.
        let cmp = p.blocks[0].insts[0].write(Reg::S(VCC)).unwrap();
        assert_eq!(
            s.bools.node(when),
            Some((Atom::Lane(cmp), Bool::TRUE, Bool::FALSE))
        );
        assert_eq!(guarded(&s), 0, "{:?}", s.guards);
    }

    #[test]
    fn exec_narrowed_without_a_branch_leaves_a_guard() {
        let p = build(
            &[
                0x7DA4_0080, // v_cmpx_eq_u32 exec, 0, v0
                0x7E02_0280, // v_mov_b32 v1, 0
                S_ENDPGM,
            ],
            true,
        );
        let s = structure(&p).unwrap();
        assert_eq!(s.guards[0][0], None, "the compare runs in every lane");
        let g = s.guards[0][1].expect("guard");
        assert!(matches!(s.bools.support(g)[..], [Atom::Lane(_)]));
    }

    #[test]
    fn divergent_loop_exits_per_lane() {
        let p = build(
            &[
                0xBE82_0480, // s_mov_b64 s[2:3], 0
                0x4A00_0081, // 4: v_add_nc_u32 v0, 1, v0
                0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
                0x8882_026A, // s_or_b64 s[2:3], vcc, s[2:3]
                0x8AFE_027E, // s_andn2_b64 exec, exec, s[2:3]
                0xBF89_FFFB, // s_cbranch_execnz -> 4
                0x88FE_027E, // s_or_b64 exec, exec, s[2:3]
                0x7E02_0280, // v_mov_b32 v1, 0
                S_ENDPGM,
            ],
            true,
        );
        let s = structure(&p).unwrap();
        assert_eq!(guarded(&s), 0, "{:?}", s.guards);
        let LaneExit::Branch {
            when,
            taken: 1,
            not_taken: 2,
        } = s.exits[1]
        else {
            panic!("{:?}", s.exits)
        };
        // Back while the lane's compare fails.
        assert!(matches!(
            s.bools.node(when),
            Some((Atom::Lane(_), Bool::TRUE, Bool::FALSE))
        ));
    }

    #[test]
    fn scalar_branches_stay_uniform() {
        let p = build(
            &[
                0xBF06_0100, // s_cmp_eq_u32 s0, s1
                0xBF85_0001, // s_cbranch_scc1 -> 12
                0x7E02_0280, // v_mov_b32 v1, 0
                S_ENDPGM,    // 12
            ],
            true,
        );
        let s = structure(&p).unwrap();
        let LaneExit::Branch { when, .. } = s.exits[0] else {
            panic!()
        };
        assert!(matches!(s.bools.support(when)[..], [Atom::Uniform(_)]));
        assert_eq!(guarded(&s), 0);
    }

    #[test]
    fn irreducible_loops_are_split() {
        let p = build(
            &[
                0xBF06_0100, // s_cmp_eq_u32 s0, s1
                0xBF85_0001, // s_cbranch_scc1 -> 12, into the loop's middle
                0x8002_8102, // 8: s_add_u32 s2, s2, 1
                0xBF84_FFFE, // 12: s_cbranch_scc0 -> 8
                S_ENDPGM,
            ],
            false,
        );
        // The loop keeps 12 as its header; the entry into 8 gets a copy of it.
        let copy = &p.blocks[4];
        assert_eq!((copy.offset, copy.exit), (8, ir::Exit::Jump(2)));
        assert_eq!(p.blocks[0].exit.successors(), [2, 4]);
        assert_eq!(p.blocks[1].preds, [2]);
        assert!(structure(&p).is_ok());
    }
