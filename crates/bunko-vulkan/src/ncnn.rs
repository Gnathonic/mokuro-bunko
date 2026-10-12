//! The ncnn runtime (`bunko-ncnn`): hayai-nova and paddle-manga with the decode loop on
//! the GPU (docs/rust-port/VULKAN.md).

use std::path::Path;

use bunko_vlm::Rgb;

use crate::backend::{DeviceKind, LoadRequest, NCNN, VkBackend, VkDevice, VkRecognizer};

pub struct Ncnn;

/// ncnn numbers the devices it keeps in the loader's order; match them to the catalog
/// by name, the n-th device of a name to the n-th catalog entry of that name.
fn map_devices(catalog: &[VkDevice]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut taken = vec![false; catalog.len()];
    for d in bunko_ncnn::devices() {
        if let Some(i) = catalog
            .iter()
            .position(|c| !taken[c.index] && c.name == d.name && c.kind != DeviceKind::Cpu)
        {
            taken[catalog[i].index] = true;
            out.push((catalog[i].index, d.index));
        }
    }
    out
}

enum Rec {
    Hayai(bunko_ncnn::NcnnHayai),
    Paddle(bunko_ncnn::NcnnPaddle),
}

impl VkRecognizer for Rec {
    fn read_crops(&self, crops: &[&Rgb], caps: Option<&[u32]>) -> Result<Vec<String>, String> {
        match self {
            Rec::Hayai(h) => h.read_crops(crops),
            Rec::Paddle(p) => {
                let default = vec![bunko_vlm::paddle::DEFAULT_MAX_NEW_TOKENS; crops.len()];
                p.read_crops(crops, caps.unwrap_or(&default))
            }
        }
        .map_err(|e| e.to_string())
    }

    fn batch(&self) -> usize {
        match self {
            Rec::Hayai(_) => bunko_vlm::hayai::BATCH,
            Rec::Paddle(_) => bunko_vlm::paddle::BATCH,
        }
    }
}

fn path<'a>(p: &'a Option<String>, what: &str) -> Result<&'a Path, String> {
    p.as_deref()
        .map(Path::new)
        .ok_or_else(|| format!("hayai-nova on ncnn needs the {what}"))
}

/// `MOKURO_NCNN_FP16=1`: fp16 storage (faster, not 0.5.2's text: opt-in only).
fn flags() -> u32 {
    if std::env::var("MOKURO_NCNN_FP16").is_ok_and(|v| v == "1") {
        bunko_ncnn::FLAG_FP16_STORAGE
    } else {
        0
    }
}

impl VkBackend for Ncnn {
    fn id(&self) -> &'static str {
        NCNN
    }

    fn version(&self) -> String {
        format!(
            "ncnn (bunko-vulkan {}, graphs format 2)",
            env!("CARGO_PKG_VERSION")
        )
    }

    fn engines(&self) -> &'static [&'static str] {
        &["hayai-nova", "paddle-manga"]
    }

    fn serves(&self, catalog: &[VkDevice]) -> Vec<usize> {
        map_devices(catalog).into_iter().map(|(c, _)| c).collect()
    }

    fn load(&self, req: &LoadRequest<'_>) -> Result<Box<dyn VkRecognizer>, String> {
        let catalog = crate::runtime::state()
            .map(|s| s.catalog.clone())
            .unwrap_or_else(|_| vec![req.device.clone()]);
        let gpu = map_devices(&catalog)
            .into_iter()
            .find(|&(c, _)| c == req.device.index)
            .map(|(_, n)| n)
            .ok_or_else(|| format!("ncnn does not see {}", req.device.name))?;
        let tokenizer = Path::new(&req.opts.tokenizer);
        let rec = match req.engine {
            "paddle-manga" => {
                let files = bunko_ncnn::PaddleFiles {
                    graphs: req.model_dir,
                    tokenizer,
                };
                Rec::Paddle(
                    bunko_ncnn::NcnnPaddle::load(&files, gpu, flags(), req.info.clone())
                        .map_err(|e| e.to_string())?,
                )
            }
            "hayai-nova" => {
                if let Some(b) = req
                    .opts
                    .patch_budget
                    .filter(|&b| b as usize != bunko_ncnn::HAYAI_BUDGET)
                {
                    return Err(format!(
                        "hayai-nova's ncnn graphs read patch budget {}, not {b}",
                        bunko_ncnn::HAYAI_BUDGET
                    ));
                }
                let files = bunko_ncnn::HayaiFiles {
                    graphs: req.model_dir,
                    pos_table: path(&req.opts.pos_table, "position table")?,
                    tokenizer,
                };
                Rec::Hayai(
                    bunko_ncnn::NcnnHayai::load(&files, gpu, flags(), req.info.clone())
                        .map_err(|e| e.to_string())?,
                )
            }
            other => return Err(format!("ncnn has no graphs for {other}")),
        };
        Ok(Box::new(rec))
    }
}
