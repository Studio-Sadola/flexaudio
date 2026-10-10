//! Silero VAD's 16 kHz ONNX inference engine (`tract-onnx`).
//!
//! The embedded model supports only 16 kHz (inputs `input` `[1,576]` f32 and `state` `[2,1,128]`
//! f32, with no `sr` input). The public API's 8 kHz input is converted to 16 kHz by the caller's
//! stateful sinc resampler before being passed here.
//!
//! Build the plan (`into_optimized` → `into_runnable`) once at startup and reuse it for each frame.
//! Carry the `[2,1,128]` state and 64-sample context across frames.

use std::io::Cursor;
use std::sync::Arc;

use tract_onnx::prelude::*;

use crate::VadError;

/// `src/silero_vad/data/silero_vad_openvino_16k.onnx` from upstream snakers4/silero-vad commit
/// `1a26f187f9dbc77d9dcaee0bfefafcc092ef7970`.
///
/// URL: https://github.com/snakers4/silero-vad/blob/1a26f187f9dbc77d9dcaee0bfefafcc092ef7970/src/silero_vad/data/silero_vad_openvino_16k.onnx
/// sha256: 7776b81ad1b0350c15d7f1555943b9232eb53e9ca5d989c6d0cea9ebc8664d87
///
/// 16 kHz only, with no If node. Inputs: `input` `[1,576]` f32 and `state` `[2,1,128]` f32.
/// There is no `sr` input. Outputs are `output` (speech probability) and `stateN`.
static MODEL_BYTES: &[u8] = include_bytes!("../assets/silero_vad_openvino_16k.onnx");

/// Sample rate expected by the model.
pub(crate) const MODEL_SAMPLE_RATE: u32 = 16_000;
/// Silero frame length at 16 kHz.
pub(crate) const MODEL_FRAME_SIZE: usize = 512;
/// Silero pre-context length at 16 kHz.
const MODEL_CONTEXT_SIZE: usize = 64;
/// Length of `concat(context, frame)` (64+512 at 16 kHz).
const MODEL_INPUT_LEN: usize = MODEL_CONTEXT_SIZE + MODEL_FRAME_SIZE;
/// Number of elements in the state tensor (`2 * 1 * 128`).
const STATE_LEN: usize = 2 * 128;

/// Inference engine holding the 16 kHz Silero graph as one optimized plan.
pub(crate) struct SileroEngine {
    plan: Arc<TypedSimplePlan>,
    /// Position of `input` in the model's input list.
    input_ix: usize,
    /// Position of `state` in the model's input list.
    state_ix: usize,
    /// Position of `output` (speech probability) in the model's output list.
    output_ix: usize,
    /// Position of `stateN` in the model's output list.
    state_out_ix: usize,
    /// Silero state tensor `[2,1,128]`, carried across frames.
    state: Vec<f32>,
    /// Context from the end of the previous frame (64 samples at 16 kHz), used as pre-context for
    /// the next frame.
    context: Vec<f32>,
}

impl SileroEngine {
    /// Checked boundary used only by the additive whisper-compatible session.
    pub(crate) fn infer_checked_16k_frame(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        let probability = self.infer_16k_frame(frame)?;
        if !probability.is_finite()
            || !(0.0..=1.0).contains(&probability)
            || self.state.iter().any(|value| !value.is_finite())
        {
            return Err(VadError::Inference(
                "nonfinite state or invalid probability".into(),
            ));
        }
        Ok(probability)
    }

    /// Optimize the embedded model with `into_optimized` and build the inference plan.
    pub(crate) fn load() -> Result<Self, VadError> {
        let (plan, input_ix, state_ix, output_ix, state_out_ix) = load_plan()?;
        Ok(SileroEngine {
            plan,
            input_ix,
            state_ix,
            output_ix,
            state_out_ix,
            state: vec![0.0f32; STATE_LEN],
            context: vec![0.0f32; MODEL_CONTEXT_SIZE],
        })
    }

    /// Run one 16 kHz frame (`MODEL_FRAME_SIZE` samples) and return its speech probability.
    ///
    /// The input is `concat(context, frame)`, not just the frame, and has length 576.
    /// After inference, update the context with the last 64 input samples and the state with output
    /// `stateN`.
    pub(crate) fn infer_16k_frame(&mut self, frame: &[f32]) -> Result<f32, VadError> {
        debug_assert_eq!(frame.len(), MODEL_FRAME_SIZE);
        debug_assert_eq!(self.context.len(), MODEL_CONTEXT_SIZE);

        let mut x = Vec::with_capacity(MODEL_INPUT_LEN);
        x.extend_from_slice(&self.context);
        x.extend_from_slice(frame);

        // Next frame's context is the last 64 samples of this input. Save them before x is moved into
        // the tensor.
        let next_context: Vec<f32> = x[MODEL_INPUT_LEN - MODEL_CONTEXT_SIZE..].to_vec();

        let in_t = Tensor::from_shape(&[1, MODEL_INPUT_LEN], &x)
            .map_err(|e| VadError::Inference(e.to_string()))?;
        let state_t = Tensor::from_shape(&[2, 1, 128], &self.state)
            .map_err(|e| VadError::Inference(e.to_string()))?;

        let mut slots: Vec<Option<TValue>> = vec![None, None];
        if self.input_ix >= slots.len() || self.state_ix >= slots.len() {
            return Err(VadError::Inference(format!(
                "unexpected input index input_ix={} state_ix={}",
                self.input_ix, self.state_ix
            )));
        }
        slots[self.input_ix] = Some(in_t.into());
        slots[self.state_ix] = Some(state_t.into());
        let mut inputs = tvec![];
        for (i, slot) in slots.into_iter().enumerate() {
            inputs.push(
                slot.ok_or_else(|| VadError::Inference(format!("tract input slot {i} empty")))?,
            );
        }

        let outputs = self
            .plan
            .run(inputs)
            .map_err(|e| VadError::Inference(e.to_string()))?;
        let prob_value = outputs.get(self.output_ix).ok_or_else(|| {
            VadError::Inference(format!(
                "output index {} for 'output' is out of range {}",
                self.output_ix,
                outputs.len()
            ))
        })?;
        let state_value = outputs.get(self.state_out_ix).ok_or_else(|| {
            VadError::Inference(format!(
                "output index {} for 'stateN' is out of range {}",
                self.state_out_ix,
                outputs.len()
            ))
        })?;
        let prob = read_probability_output(prob_value)?;
        let new_state = read_state_output(state_value)?;
        self.state = new_state;
        self.context = next_context;
        Ok(prob)
    }

    /// Zero-initialize the state and context.
    pub(crate) fn reset(&mut self) {
        self.state.fill(0.0);
        self.context.fill(0.0);
    }
}

fn load_plan() -> Result<(Arc<TypedSimplePlan>, usize, usize, usize, usize), VadError> {
    let mut model = tract_onnx::onnx()
        .model_for_read(&mut Cursor::new(MODEL_BYTES))
        .map_err(|e| VadError::ModelLoad(format!("{e:#}")))?;
    let (input_ix, state_ix) = locate_silero_inputs(&model)?;
    let (output_ix, state_out_ix) = locate_silero_outputs(&model)?;
    model = model
        .with_input_fact(input_ix, f32::fact([1usize, MODEL_INPUT_LEN]).into())
        .map_err(|e| VadError::ModelLoad(format!("{e:#}")))?;
    model = model
        .with_input_fact(state_ix, f32::fact([2usize, 1usize, 128usize]).into())
        .map_err(|e| VadError::ModelLoad(format!("{e:#}")))?;
    // Spell out the same Typed optimization path used inside `into_optimized`. tract 0.23.7's debug
    // checks reject duplicate temporary Const names created when expanding this model's Scan, so
    // make names unique at each stage. Build the plan once here; do not re-optimize per frame.
    uniquify_node_names(&mut model);
    let mut typed = model
        .into_typed()
        .map_err(|e| VadError::ModelLoad(format!("{e:#}")))?;
    uniquify_typed_names(&mut typed);
    accept_debug_duplicate_names(typed.declutter())?;
    uniquify_typed_names(&mut typed);
    accept_debug_duplicate_names(typed.optimize())?;
    uniquify_typed_names(&mut typed);
    let plan = typed
        .into_runnable()
        .map_err(|e| VadError::ModelLoad(format!("{e:#}")))?;
    Ok((plan, input_ix, state_ix, output_ix, state_out_ix))
}

fn uniquify_node_names(model: &mut InferenceModel) {
    uniquify_names(&mut model.nodes);
}

fn uniquify_typed_names(model: &mut TypedModel) {
    uniquify_names(&mut model.nodes);
}

fn uniquify_names<F: Fact, O>(nodes: &mut [Node<F, O>]) {
    let mut seen = std::collections::HashSet::new();
    for (ix, node) in nodes.iter_mut().enumerate() {
        if !seen.insert(node.name.clone()) {
            node.name = format!("{}#{ix}", node.name);
            seen.insert(node.name.clone());
        }
    }
}

/// tract 0.23.7 checks node-name uniqueness after debug `compact()`. If Silero's Scan expansion
/// creates duplicate temporary Const names, accept the optimization result that has already finished.
fn accept_debug_duplicate_names(result: TractResult<()>) -> Result<(), VadError> {
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("duplicate name") {
                Ok(())
            } else {
                Err(VadError::ModelLoad(msg))
            }
        }
    }
}

fn locate_silero_inputs(model: &InferenceModel) -> Result<(usize, usize), VadError> {
    let outlets = model
        .input_outlets()
        .map_err(|e| VadError::ModelLoad(format!("{e:#}")))?
        .to_vec();
    if outlets.len() != 2 {
        return Err(VadError::ModelLoad(format!(
            "expected 2 inputs (input, state), got {}",
            outlets.len()
        )));
    }
    let mut input_ix = None;
    let mut state_ix = None;
    for (ix, outlet) in outlets.iter().enumerate() {
        let name = model.node(outlet.node).name.as_str();
        match name {
            "input" => input_ix = Some(ix),
            "state" => state_ix = Some(ix),
            other => {
                return Err(VadError::ModelLoad(format!(
                    "unexpected model input {other:?}"
                )))
            }
        }
    }
    match (input_ix, state_ix) {
        (Some(i), Some(s)) => Ok((i, s)),
        _ => Err(VadError::ModelLoad(
            "model inputs must be named 'input' and 'state'".to_string(),
        )),
    }
}

fn locate_silero_outputs(model: &InferenceModel) -> Result<(usize, usize), VadError> {
    let outlets = model
        .output_outlets()
        .map_err(|e| VadError::ModelLoad(format!("{e:#}")))?
        .to_vec();
    if outlets.len() != 2 {
        return Err(VadError::ModelLoad(format!(
            "expected 2 outputs (output, stateN), got {}",
            outlets.len()
        )));
    }
    let mut output_ix = None;
    let mut state_out_ix = None;
    for (ix, outlet) in outlets.iter().enumerate() {
        let name = model.outlet_label(*outlet).ok_or_else(|| {
            VadError::ModelLoad(format!("model output {ix} has no ONNX output name"))
        })?;
        match name {
            "output" if output_ix.is_none() => output_ix = Some(ix),
            "stateN" if state_out_ix.is_none() => state_out_ix = Some(ix),
            "output" | "stateN" => {
                return Err(VadError::ModelLoad(format!(
                    "duplicate model output name {name:?}"
                )))
            }
            other => {
                return Err(VadError::ModelLoad(format!(
                    "unexpected model output {other:?}"
                )))
            }
        }
    }
    match (output_ix, state_out_ix) {
        (Some(output), Some(state)) => Ok((output, state)),
        _ => Err(VadError::ModelLoad(
            "model outputs must be named 'output' and 'stateN'".to_string(),
        )),
    }
}

fn read_probability_output(value: &TValue) -> Result<f32, VadError> {
    let view = value
        .to_plain_array_view::<f32>()
        .map_err(|e| VadError::Inference(format!("output view: {e}")))?;
    if view.len() != 1 {
        return Err(VadError::Inference(format!(
            "unexpected 'output' length {} (expected 1)",
            view.len()
        )));
    }
    view.iter()
        .next()
        .copied()
        .ok_or_else(|| VadError::Inference("empty 'output' tensor".to_string()))
}

fn read_state_output(value: &TValue) -> Result<Vec<f32>, VadError> {
    let view = value
        .to_plain_array_view::<f32>()
        .map_err(|e| VadError::Inference(format!("stateN view: {e}")))?;
    if view.len() != STATE_LEN {
        return Err(VadError::Inference(format!(
            "unexpected 'stateN' length {} (expected {STATE_LEN})",
            view.len()
        )));
    }
    Ok(view.iter().copied().collect())
}
