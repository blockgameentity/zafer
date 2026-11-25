//! The shader's interface: the console's input and output signatures as
//! SPIR-V locations and built-ins.
//!
//! A console shader names its rows by hardware register (`v0`, `v2`,
//! `exp param0`), not by a location. The host pipeline needs locations, so
//! the signature rows become consecutive locations in the order the
//! container lists them, and a row that is a system value becomes the
//! matching built-in. The pipeline object created from the same signature
//! uses the same numbering, so the two agree.
//!
//! The pull model's barycentrics are not a vertex attribute: they are
//! `gl_BaryCoord*`, which a shader reading them takes as a built-in.

use std::collections::HashMap;

use victory_shader::io::{Interp, Io, Target};
use victory_shader::native::HwStage;
use victory_spirv::enums::BuiltIn;

use crate::module::{Module, Stage, Variable};

/// One interface row: where it comes from, where it goes and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// The location the emitter writes or reads, or `None` for a built-in.
    pub location: Option<u32>,
    pub builtin: Option<BuiltIn>,
    /// The variable and its type.
    pub variable: Variable,
    /// Components the row carries (1-4).
    pub components: u8,
}

/// What an output is, so a later export finds it again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Color(u8),
    Depth,
    Position(u8),
    Param { row: u8 },
    Primitive,
}

/// Every interface row of a shader, by what produces and consumes it.
#[derive(Default)]
pub struct Interface {
    /// Vertex attributes by signature row and channel.
    attributes: HashMap<(u8, u8), Row>,
    /// Pixel: interpolated inputs by signature row and channel.
    pixel_inputs: HashMap<(u8, u8), Row>,
    /// Outputs by export target.
    outputs: HashMap<Key, Row>,
    /// `gl_Position`.
    pub position: Option<Row>,
    /// `gl_FragCoord`.
    pub frag_coord: Option<Row>,
    /// `gl_FragDepth`.
    pub frag_depth: Option<Row>,
    /// `gl_VertexIndex` and the other system values.
    builtins: HashMap<BuiltIn, Row>,
    /// The next free location.
    next: u32,
}

impl Interface {
    /// Declares the interface of `io` for `stage` in `module`.
    ///
    /// The numbering is the signature's order, so a pipeline built from the
    /// same signature matches without a second description of it.
    pub fn build(module: &mut Module, stage: Stage, io: &Io) -> Interface {
        let mut this = Interface::default();
        if stage == Stage::Pixel {
            this.declare_fragment_inputs(module, io);
        }
        if stage == Stage::Vertex {
            this.declare_vertex_inputs(module, io);
        }
        this.declare_outputs(module, stage, io);
        this
    }

    /// Pixel inputs: `gl_FragCoord` and each interpolated attribute.
    fn declare_fragment_inputs(&mut self, module: &mut Module, io: &Io) {
        let v4 = module.vec(module.f32_ty, 4);
        let row = module.builtin(BuiltIn::FragCoord, v4);
        self.frag_coord = Some(Row {
            location: None,
            builtin: Some(BuiltIn::FragCoord),
            variable: row,
            components: 4,
        });
        let mut seen: Vec<(u8, u8)> = Vec::new();
        for (_, interp) in &io.interps {
            let attribute = match interp {
                Interp::Interpolated { attribute, .. } | Interp::Vertex { attribute, .. } => {
                    *attribute
                }
            };
            let key = (attribute.row, attribute.channel);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let location = self.next;
            self.next += 1;
            let variable = module.input_at(location, v4);
            self.pixel_inputs.insert(
                key,
                Row {
                    location: Some(location),
                    builtin: None,
                    variable,
                    components: 4,
                },
            );
        }
    }

    /// Vertex inputs: `gl_VertexIndex` and each attribute the fetch shader
    /// loads, which the pipeline supplies as vertex buffers.
    fn declare_vertex_inputs(&mut self, module: &mut Module, io: &Io) {
        let index = module.builtin(BuiltIn::VertexIndex, module.u32_ty);
        self.builtins.insert(
            BuiltIn::VertexIndex,
            Row {
                location: None,
                builtin: Some(BuiltIn::VertexIndex),
                variable: index,
                components: 1,
            },
        );
        let mut seen: Vec<(u8, u8)> = Vec::new();
        for (_, input) in &io.inputs {
            let victory_shader::io::Input::Attribute { row, channel } = input else {
                continue;
            };
            let key = (*row, *channel);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let location = self.next;
            self.next += 1;
            let v4 = module.vec(module.f32_ty, 4);
            let variable = module.input_at(location, v4);
            self.attributes.insert(
                key,
                Row {
                    location: Some(location),
                    builtin: None,
                    variable,
                    components: 4,
                },
            );
        }
    }

    /// Outputs: the position, the depth, colour targets and parameters.
    fn declare_outputs(&mut self, module: &mut Module, stage: Stage, io: &Io) {
        let v4 = module.vec(module.f32_ty, 4);
        if stage == Stage::Vertex {
            let row = module.builtin(BuiltIn::Position, v4);
            self.position = Some(Row {
                location: None,
                builtin: Some(BuiltIn::Position),
                variable: row,
                components: 4,
            });
        }
        let mut seen: Vec<Target> = Vec::new();
        for export in &io.exports {
            if seen.contains(&export.target) {
                continue;
            }
            seen.push(export.target);
            let Some(key) = key_of(&export.target) else {
                continue;
            };
            if matches!(key, Key::Position(_)) {
                continue;
            }
            if key == Key::Depth {
                let row = module.builtin(BuiltIn::FragDepth, module.f32_ty);
                self.frag_depth = Some(Row {
                    location: None,
                    builtin: Some(BuiltIn::FragDepth),
                    variable: row,
                    components: 1,
                });
                continue;
            }
            let location = match key {
                Key::Color(n) => u32::from(n),
                _ => {
                    let l = self.next;
                    self.next += 1;
                    l
                }
            };
            let variable = module.output_at(location, v4);
            self.outputs.insert(
                key,
                Row {
                    location: Some(location),
                    builtin: None,
                    variable,
                    components: 4,
                },
            );
        }
    }

    /// The row an export of `target` writes.
    pub fn output(&self, target: &Target) -> Option<&Row> {
        self.outputs.get(&key_of(target)?)
    }

    /// The input a vertex attribute is fetched into.
    pub fn attribute(&self, row: u8, channel: u8) -> Option<&Row> {
        self.attributes.get(&(row, channel))
    }

    /// The interpolated input of a pixel shader.
    pub fn interpolated(&self, row: u8, channel: u8) -> Option<&Row> {
        self.pixel_inputs.get(&(row, channel))
    }

    /// A built-in by which one.
    pub fn builtin(&self, builtin: BuiltIn) -> Option<&Row> {
        self.builtins.get(&builtin)
    }
}

/// The key an export target maps to, `None` for one with no host output.
fn key_of(target: &Target) -> Option<Key> {
    match target {
        Target::Color(n) => Some(Key::Color(*n)),
        Target::Depth => Some(Key::Depth),
        Target::Position(n) => Some(Key::Position(*n)),
        Target::Param { row, .. } | Target::PrimitiveParam { row, .. } => {
            Some(Key::Param { row: *row })
        }
        Target::Primitive => Some(Key::Primitive),
        Target::Null | Target::Unread => None,
    }
}

/// Whether a stage writes colour.
pub fn writes_color(stage: HwStage) -> bool {
    stage == HwStage::Ps
}
