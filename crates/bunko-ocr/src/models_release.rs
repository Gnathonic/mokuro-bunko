//! The `models-v1` release set (written by `tools/onnx_export`, `build_manifest.py`;
//! `docs/rust-port/MODELS.md`): the hayai-nova and paddle-manga ONNX exports and their
//! host tables, published as flat-named GitHub release assets. Generated from that
//! release's `models.json` (export tool 1.0.0); a new export is a new release tag and a
//! new table here, never a changed row.
//!
//! Columns: manifest id, release file name, engine, role, precision, the `.onnx` an
//! external-data file belongs to, size, sha256.

/// The release tag the files come from.
pub const RELEASE: &str = "models-v1";
/// Where release assets are downloaded from.
pub const RELEASE_BASE_URL: &str =
    "https://github.com/Gnathonic/mokuro-bunko/releases/download/models-v1";

/// One row of the release table.
pub struct ReleaseFile {
    pub id: &'static str,
    pub file: &'static str,
    pub engine: &'static str,
    pub role: &'static str,
    pub precision: Option<&'static str>,
    pub part_of: Option<&'static str>,
    pub size: u64,
    pub sha256: &'static str,
}

/// PP-OCR files as the release names them (copied as-is from the pinned repo, so their
/// sha256 is the builtin manifest's): `(manifest id, release file)`.
pub const PPOCR_RELEASE_NAMES: [(&str, &str); 3] = [
    ("ppocr-manga/det-v0.2", "ppocr-manga_det_v0.2.onnx"),
    ("ppocr-manga/rec-v0.2", "ppocr-manga_rec_v0.2.onnx"),
    ("ppocr-manga/dict-v6", "ppocr-manga_dict.txt"),
];

/// Every VLM file of the release.
pub const FILES: &[ReleaseFile] = &[
    ReleaseFile {
        id: "hayai-nova/vision-fp32",
        file: "hayai-nova_vision_fp32.onnx",
        engine: "hayai-nova",
        role: "vision",
        precision: Some("fp32"),
        part_of: None,
        size: 350_519_569,
        sha256: "5e3823d76c1826911793c15288850a57ced3d183f82ea9e0493299e917aaa7a4",
    },
    ReleaseFile {
        id: "hayai-nova/decoder-fp32",
        file: "hayai-nova_decoder_fp32.onnx",
        engine: "hayai-nova",
        role: "decoder",
        precision: Some("fp32"),
        part_of: None,
        size: 216_611_483,
        sha256: "d6e4677de36941164bbadb8d10a4ab72d62ae1d199eb635cb7c1777ae339cf22",
    },
    ReleaseFile {
        id: "hayai-nova/vision-fp16",
        file: "hayai-nova_vision_fp16.onnx",
        engine: "hayai-nova",
        role: "vision",
        precision: Some("fp16"),
        part_of: None,
        size: 175_548_522,
        sha256: "4ea6f18cc4c5ff49d36f29d4503708a508d573c3efd4f8d1d3b3b673b8bd811f",
    },
    ReleaseFile {
        id: "hayai-nova/decoder-fp16",
        file: "hayai-nova_decoder_fp16.onnx",
        engine: "hayai-nova",
        role: "decoder",
        precision: Some("fp16"),
        part_of: None,
        size: 109_020_272,
        sha256: "3f12a09559086ac478c52b15dee327d1d7c5f9f695255a119798a88fad69773a",
    },
    ReleaseFile {
        id: "hayai-nova/pos-table",
        file: "hayai-nova_pos_table.npy",
        engine: "hayai-nova",
        role: "pos_table",
        precision: Some("fp32"),
        part_of: None,
        size: 786_560,
        sha256: "d91476488d133da38b9908b049975173e024d73e16db8b3aba1b213cee298f03",
    },
    ReleaseFile {
        id: "hayai-nova/token-embeddings",
        file: "hayai-nova_token_embeddings.npy",
        engine: "hayai-nova",
        role: "token_embeddings",
        precision: Some("fp32"),
        part_of: None,
        size: 32_776_320,
        sha256: "9a86508db3783874c4e1e603d4cfd95f98cdee90d95a14dbcfed9ad0bf057efd",
    },
    ReleaseFile {
        id: "hayai-nova/tokenizer",
        file: "hayai-nova_tokenizer.json",
        engine: "hayai-nova",
        role: "tokenizer",
        precision: None,
        part_of: None,
        size: 1_247_253,
        sha256: "f8a0a909c628a684fe463094614e236a8b1d3609e7770f77e7beafaf1056bf13",
    },
    ReleaseFile {
        id: "hayai-nova/config",
        file: "hayai-nova_config.json",
        engine: "hayai-nova",
        role: "config",
        precision: None,
        part_of: None,
        size: 549,
        sha256: "9f7457f50fe0a54de26dfa3eb8114bcaa470ff55a0f614ec0599b8caa63d96d0",
    },
    ReleaseFile {
        id: "paddle-manga/vision-fp32",
        file: "paddle-manga_vision_fp32.onnx",
        engine: "paddle-manga",
        role: "vision",
        precision: Some("fp32"),
        part_of: None,
        size: 1_445_621,
        sha256: "dc7c05af5cd6ae1cf8f25cd42b09da9a0d3a7424222728ac26cd0c363b8d9e1e",
    },
    ReleaseFile {
        id: "paddle-manga/vision-data-fp32",
        file: "paddle-manga_vision_fp32.onnx.data",
        engine: "paddle-manga",
        role: "vision",
        precision: Some("fp32"),
        part_of: Some("paddle-manga_vision_fp32.onnx"),
        size: 1_755_789_760,
        sha256: "08d8cd0a1fb1c8baca58678d8689062534e64aca915d51c27f34a219b1c7006c",
    },
    ReleaseFile {
        id: "paddle-manga/decoder-fp32",
        file: "paddle-manga_decoder_fp32.onnx",
        engine: "paddle-manga",
        role: "decoder",
        precision: Some("fp32"),
        part_of: None,
        size: 1_370_555,
        sha256: "573a50b1a066b1708506e6d8bb240a9d7b61f2f9caca94f9e49b710b4546f38f",
    },
    ReleaseFile {
        id: "paddle-manga/decoder-data-fp32",
        file: "paddle-manga_decoder_fp32.onnx.data",
        engine: "paddle-manga",
        role: "decoder",
        precision: Some("fp32"),
        part_of: Some("paddle-manga_decoder_fp32.onnx"),
        size: 1_442_992_128,
        sha256: "2d1c66ad23a9606fbfada9ede9aa68b59cd3116178564d7cb788cb5e1a08a4cd",
    },
    ReleaseFile {
        id: "paddle-manga/embed-fp32",
        file: "paddle-manga_embed_fp32.npy",
        engine: "paddle-manga",
        role: "embed",
        precision: Some("fp32"),
        part_of: None,
        size: 423_624_832,
        sha256: "d7f46e3ea1f07125b088dd37ceeccefc081bc19b22fee747b628e01df5bf43a9",
    },
    ReleaseFile {
        id: "paddle-manga/vision-fp16",
        file: "paddle-manga_vision_fp16.onnx",
        engine: "paddle-manga",
        role: "vision",
        precision: Some("fp16"),
        part_of: None,
        size: 1_530_186,
        sha256: "a2d562e218223b629d8963e7a7f72c7dc37e4c8ef3303b1383ba71d196088bef",
    },
    ReleaseFile {
        id: "paddle-manga/vision-data-fp16",
        file: "paddle-manga_vision_fp16.onnx.data",
        engine: "paddle-manga",
        role: "vision",
        precision: Some("fp16"),
        part_of: Some("paddle-manga_vision_fp16.onnx"),
        size: 877_895_744,
        sha256: "7c6776b7cb98747501caffb9900ecd101fcaa86cd8b0609ec67de26d2fca378a",
    },
    ReleaseFile {
        id: "paddle-manga/decoder-fp16",
        file: "paddle-manga_decoder_fp16.onnx",
        engine: "paddle-manga",
        role: "decoder",
        precision: Some("fp16"),
        part_of: None,
        size: 1_457_698,
        sha256: "5c47509d95dec391d543ae9929a60cb0f5e23a9e1efd28bc4c4e59da92aa545a",
    },
    ReleaseFile {
        id: "paddle-manga/decoder-data-fp16",
        file: "paddle-manga_decoder_fp16.onnx.data",
        engine: "paddle-manga",
        role: "decoder",
        precision: Some("fp16"),
        part_of: Some("paddle-manga_decoder_fp16.onnx"),
        size: 721_496_064,
        sha256: "7843113600b6552e0ac5c2453f38b71c9311802e8f8378f92444a0c7db58ba18",
    },
    ReleaseFile {
        id: "paddle-manga/embed-fp16",
        file: "paddle-manga_embed_fp16.npy",
        engine: "paddle-manga",
        role: "embed",
        precision: Some("fp16"),
        part_of: None,
        size: 211_812_480,
        sha256: "2ebd015d3e28397a956386ca8cc1699b1914aa2aac5ec40436f74bf3e84b7e4e",
    },
    ReleaseFile {
        id: "paddle-manga/tokenizer",
        file: "paddle-manga_tokenizer.json",
        engine: "paddle-manga",
        role: "tokenizer",
        precision: None,
        part_of: None,
        size: 11_189_060,
        sha256: "c8a215a59183d0d0781adc33bacd3ce6162716f7fd568fb30234a74d69803a7d",
    },
    ReleaseFile {
        id: "paddle-manga/config",
        file: "paddle-manga_config.json",
        engine: "paddle-manga",
        role: "config",
        precision: None,
        part_of: None,
        size: 978,
        sha256: "322649b1722457bb49cadc5cbb2ccd49a7fdbc1f9f88a243980ffc2b62385ffd",
    },
];
