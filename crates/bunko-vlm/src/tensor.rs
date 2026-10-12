//! Host-side tensor helpers over `ort` values.

use half::f16;
use ort::memory::Allocator;
use ort::value::{DynTensor, DynValue, Tensor, TensorElementType};

use crate::VlmError;

fn rt(e: ort::Error) -> VlmError {
    VlmError::Runtime(e.to_string())
}

/// Float format of a graph input/output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FloatKind {
    F32,
    F16,
}

impl FloatKind {
    pub(crate) fn of(t: Option<TensorElementType>, what: &str) -> Result<Self, VlmError> {
        match t {
            Some(TensorElementType::Float32) => Ok(FloatKind::F32),
            Some(TensorElementType::Float16) => Ok(FloatKind::F16),
            other => Err(VlmError::Asset(format!(
                "{what}: unexpected element type {other:?}"
            ))),
        }
    }

    fn elem(self) -> TensorElementType {
        match self {
            FloatKind::F32 => TensorElementType::Float32,
            FloatKind::F16 => TensorElementType::Float16,
        }
    }
}

/// A float tensor in the graph's format (f32 data cast to f16 round-to-nearest-even,
/// like numpy's `astype(float16)`).
pub(crate) fn floats(
    kind: FloatKind,
    shape: &[usize],
    data: Vec<f32>,
) -> Result<DynValue, VlmError> {
    debug_assert_eq!(shape.iter().product::<usize>(), data.len());
    match kind {
        FloatKind::F32 => Ok(Tensor::from_array((shape.to_vec(), data))
            .map_err(rt)?
            .into_dyn()),
        FloatKind::F16 => {
            let h: Vec<f16> = data.into_iter().map(f16::from_f32).collect();
            Ok(Tensor::from_array((shape.to_vec(), h))
                .map_err(rt)?
                .into_dyn())
        }
    }
}

/// An f16 tensor from f16 data (no round trip through f32).
pub(crate) fn halfs(shape: &[usize], data: Vec<f16>) -> Result<DynValue, VlmError> {
    Ok(Tensor::from_array((shape.to_vec(), data))
        .map_err(rt)?
        .into_dyn())
}

pub(crate) fn i64s(shape: &[usize], data: Vec<i64>) -> Result<DynValue, VlmError> {
    Ok(Tensor::from_array((shape.to_vec(), data))
        .map_err(rt)?
        .into_dyn())
}

/// An uninitialised tensor from `alloc` (device memory for outputs that stay on the GPU).
pub(crate) fn alloc(
    alloc: &Allocator,
    kind: FloatKind,
    shape: &[usize],
) -> Result<DynValue, VlmError> {
    let dims: Vec<i64> = shape.iter().map(|&d| d as i64).collect();
    Ok(
        DynTensor::new(alloc, kind.elem(), ort::value::Shape::new(dims))
            .map_err(rt)?
            .into_dyn(),
    )
}

/// A float output as f32 (f16 widened).
pub(crate) fn to_f32(v: &DynValue) -> Result<Vec<f32>, VlmError> {
    match v.dtype().tensor_type() {
        Some(TensorElementType::Float32) => {
            Ok(v.try_extract_tensor::<f32>().map_err(rt)?.1.to_vec())
        }
        Some(TensorElementType::Float16) => Ok(v
            .try_extract_tensor::<f16>()
            .map_err(rt)?
            .1
            .iter()
            .map(|h| h.to_f32())
            .collect()),
        other => Err(VlmError::Runtime(format!(
            "expected a float output, got {other:?}"
        ))),
    }
}

/// Index of the first maximum of each `n`-wide row (numpy/torch `argmax`).
pub(crate) fn argmax_rows(v: &[f32], n: usize) -> Vec<u32> {
    v.chunks_exact(n)
        .map(|row| {
            let mut best = 0;
            for (i, &x) in row.iter().enumerate() {
                if x > row[best] {
                    best = i;
                }
            }
            best as u32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argmax_first_of_ties() {
        assert_eq!(argmax_rows(&[1.0, 3.0, 3.0, 0.0, 0.0, 0.0], 3), vec![1, 0]);
    }
}
