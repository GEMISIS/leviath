//! `RunGraph`'s and `RegionDef`'s two forms: every field in a readable
//! format, and the binary one the run file keeps.
//!
//! The binary frames have the fields each type had before it took the
//! settings a continued run reads (`continue_at` on the graph,
//! `on_continue` on a region) and the one a media stage reads
//! (`prompt_regions` on a stage), so run files keep their layout. A value that
//! sets one writes the slot it rides in (the graph's `tool_rescan`, the
//! region's `admission`, the stage's `mode`) as the variant added at the end of that slot's
//! enum, which carries both. A value that sets neither is written exactly as
//! before, byte for byte.

use leviath_core::policy::ToolPolicy;
use leviath_core::region::{Admission, Volatility};
use serde::{Deserialize, Serialize};

use super::mode::ModeFrame;
use super::{WriteWith, pick};
use crate::spec::graph::{
    Budget, CompactionDef, ContextTransformDef, DependencyDef, EdgeDef, FileTrackingDef,
    McpServerDef, MimeRows, NudgeDef, OnContinue, OutputDef, RegionDef, RegionKind,
    RegionLayoutDef, RepetitionDef, RoutingDef, RunGraph, SafeCommandsDef, SandboxDef,
    ScriptPermissionsDef, Seed, StageDef, ToolRescan,
};
use crate::spec::inputs::InputDecl;
use crate::spec::names::{MimePattern, RegionName, StageName, ToolName};

/// The `tool_rescan` slot of a graph's binary frame.
///
/// Its first three variants keep the order and shape `ToolRescan` has
/// always been written in. The variant carrying `continue_at` is added at
/// the end, so a file written before it existed reads the same.
#[derive(Serialize, Deserialize)]
enum RescanSlot {
    AtSpawn,
    AfterWrites,
    BeforeDispatch,
    Continue {
        tool_rescan: ToolRescan,
        continue_at: StageName,
    },
}

impl RescanSlot {
    fn of(tool_rescan: ToolRescan, continue_at: Option<&StageName>) -> Self {
        match (tool_rescan, continue_at) {
            (tool_rescan, Some(at)) => Self::Continue {
                tool_rescan,
                continue_at: at.clone(),
            },
            (ToolRescan::AtSpawn, None) => Self::AtSpawn,
            (ToolRescan::AfterWrites, None) => Self::AfterWrites,
            (ToolRescan::BeforeDispatch, None) => Self::BeforeDispatch,
        }
    }

    fn split(self) -> (ToolRescan, Option<StageName>) {
        match self {
            Self::AtSpawn => (ToolRescan::AtSpawn, None),
            Self::AfterWrites => (ToolRescan::AfterWrites, None),
            Self::BeforeDispatch => (ToolRescan::BeforeDispatch, None),
            Self::Continue {
                tool_rescan,
                continue_at,
            } => (tool_rescan, Some(continue_at)),
        }
    }
}

/// The `admission` slot of a region's binary frame: `Admission`'s two
/// variants in their order, the one carrying `on_continue` after them, and
/// the one carrying `evict_to` (with whatever `on_continue` says) last.
///
/// Each later variant was added at the end, so a run file written before it
/// existed still reads: a frame's variant index is its wire form.
#[derive(Serialize, Deserialize)]
enum AdmissionSlot {
    Evict,
    Reject,
    Continue {
        admission: Admission,
        on_continue: OnContinue,
    },
    EvictTo {
        admission: Admission,
        on_continue: Option<OnContinue>,
        evict_to: f64,
    },
}

impl AdmissionSlot {
    fn of(admission: Admission, on_continue: Option<OnContinue>, evict_to: Option<f64>) -> Self {
        match (admission, on_continue, evict_to) {
            (admission, on_continue, Some(evict_to)) => Self::EvictTo {
                admission,
                on_continue,
                evict_to,
            },
            (admission, Some(on_continue), None) => Self::Continue {
                admission,
                on_continue,
            },
            (Admission::Evict, None, None) => Self::Evict,
            (Admission::Reject, None, None) => Self::Reject,
        }
    }

    fn split(self) -> (Admission, Option<OnContinue>, Option<f64>) {
        match self {
            Self::Evict => (Admission::Evict, None, None),
            Self::Reject => (Admission::Reject, None, None),
            Self::Continue {
                admission,
                on_continue,
            } => (admission, Some(on_continue), None),
            Self::EvictTo {
                admission,
                on_continue,
                evict_to,
            } => (admission, on_continue, Some(evict_to)),
        }
    }
}

/// A binary frame for `$ty`: the fields named in `before` and `after`, in
/// that order, with `$slot` between them carrying the settings the frame
/// leaves out. `$to_slot` builds the slot from the value; its `split` gives
/// back the fields it carries, named in order.
macro_rules! frame {
    (
        $ty:ident, $frame:ident, $frame_ref:ident,
        before { $($b:ident: $bt:ty),* $(,)? }
        $slot:ident: $slot_ty:ty = $to_slot:expr, ($($s:ident),+ $(,)?),
        after { $($a:ident: $at:ty),* $(,)? }
    ) => {
        #[derive(Serialize)]
        struct $frame_ref<'a> {
            $($b: &'a $bt,)*
            $slot: $slot_ty,
            $($a: &'a $at,)*
        }

        #[derive(Deserialize)]
        struct $frame {
            $($b: $bt,)*
            $slot: $slot_ty,
            $($a: $at,)*
        }

        impl<'a> From<&'a $ty> for $frame_ref<'a> {
            fn from(v: &'a $ty) -> Self {
                let to_slot: fn(&$ty) -> $slot_ty = $to_slot;
                Self {
                    $($b: &v.$b,)*
                    $slot: to_slot(v),
                    $($a: &v.$a,)*
                }
            }
        }

        impl From<$frame> for $ty {
            fn from(f: $frame) -> Self {
                let ($($s,)+) = f.$slot.split();
                Self {
                    $($b: f.$b,)*
                    $($s,)+
                    $($a: f.$a,)*
                }
            }
        }

        impl Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let frame: WriteWith<Self, S> = |v, s| $frame_ref::from(v).serialize(s);
                let text: WriteWith<Self, S> = |v, s| $ty::serialize(v, s);
                pick(s.is_human_readable(), frame, text)(self, s)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let frame: fn(D) -> Result<Self, D::Error> =
                    |d| $frame::deserialize(d).map(Self::from);
                let text: fn(D) -> Result<Self, D::Error> = |d| $ty::deserialize(d);
                pick(d.is_human_readable(), frame, text)(d)
            }
        }
    };
}

frame!(
    RunGraph, GraphFrame, GraphFrameRef,
    before {
        title: Option<String>,
        description: Option<String>,
        entry: Option<StageName>,
        stages: Vec<StageDef>,
        edges: Vec<EdgeDef>,
        layout: RegionLayoutDef,
        inputs: Vec<InputDecl>,
        output: Option<OutputDef>,
        compaction: Option<CompactionDef>,
        max_child_depth: Option<u8>,
        taint_tracking: Option<bool>,
        tool_permissions: std::collections::BTreeMap<ToolName, ToolPolicy>,
        sandbox: Option<SandboxDef>,
        read_paths: Vec<String>,
        write_paths: Vec<String>,
        safe_commands: SafeCommandsDef,
        batch_tool_hint: Option<bool>,
        shell_hint: Option<bool>,
        nudge: Option<NudgeDef>,
        routing: Option<RoutingDef>,
        repetition: Option<RepetitionDef>,
        file_tracking: Option<FileTrackingDef>,
    }
    tool_rescan: RescanSlot = |g| RescanSlot::of(g.tool_rescan, g.continue_at.as_ref()),
        (tool_rescan, continue_at),
    after {
        transforms: Vec<ContextTransformDef>,
        mime_types: MimeRows,
        dependencies: Vec<DependencyDef>,
        mcp_servers: Vec<McpServerDef>,
        script_permissions: ScriptPermissionsDef,
    }
);

frame!(
    RegionDef, RegionFrame, RegionFrameRef,
    before {
        name: RegionName,
        kind: RegionKind,
        budget: Budget,
        compact_at: Option<f64>,
        description: Option<String>,
        describe_in_prompt: bool,
        required: bool,
        required_message: Option<String>,
        summarizable: bool,
    }
    admission: AdmissionSlot = |r| AdmissionSlot::of(r.admission, r.on_continue, r.evict_to),
        (admission, on_continue, evict_to),
    after {
        volatility: Volatility,
        seed: Option<Seed>,
        accepts: Vec<MimePattern>,
    }
);

frame!(
    StageDef, StageFrame, StageFrameRef,
    before {
        name: StageName,
        description: Option<String>,
        system_prompt: Option<String>,
        model: crate::spec::graph::ModelChoice,
        tools: Vec<crate::spec::graph::ToolSelector>,
        required_tools: Vec<ToolName>,
        connectors: Vec<crate::spec::names::McpServerName>,
        max_iterations: Option<u32>,
    }
    mode: ModeFrame = |s| ModeFrame::of(&s.mode, &s.prompt_regions),
        (mode, prompt_regions),
    after {
        layout: Option<RegionLayoutDef>,
        hide: Vec<RegionName>,
        reset: Vec<RegionName>,
        tool_permissions: std::collections::BTreeMap<ToolName, ToolPolicy>,
        requires_children: bool,
        max_revisits: Option<u32>,
        transition_prompt: Option<String>,
        routing: Option<RoutingDef>,
        accepts_messages: bool,
        allow_complete: bool,
        allow_as_worker: bool,
        allow_blocking_tools: bool,
        taint_tracking: Option<bool>,
        batch_tool_hint: Option<bool>,
        shell_hint: Option<bool>,
        nudge: Option<NudgeDef>,
        sandbox: Option<SandboxDef>,
        tool_routing: Option<crate::spec::graph::ToolRoutingDef>,
        output_routing: std::collections::BTreeMap<String, RegionName>,
        output: Option<OutputDef>,
        require_output: bool,
        input_accepts: Vec<MimePattern>,
        input_as_text: Vec<MimePattern>,
        tool_accepts: std::collections::BTreeMap<ToolName, Vec<MimePattern>>,
        write_paths: Vec<String>,
        hooks: crate::spec::graph::StageHooks,
    }
);
