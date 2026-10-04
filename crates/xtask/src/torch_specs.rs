//! What goes into each OCR backend pack (`xtask torch-pack`): the pinned upstream
//! libtorch 2.13.0 distribution, the files kept from it, and the NVIDIA libraries the
//! installer fetches from PyPI. See docs/rust-port/PACKAGING.md §8 for how these lists
//! were found (ldd/readelf closure + LD_DEBUG=libs on real OCR runs).
//!
//! Every URL is pinned by sha256 and size. A new libtorch or CUDA is a new table here.

/// libtorch version every pack is built from (= `tch` 0.26).
pub const TORCH_VERSION: &str = "2.13.0";

/// A pinned download.
#[derive(Debug, Clone, Copy)]
pub struct Upstream {
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

/// How the libtorch files are laid out in the upstream archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// download.pytorch.org `libtorch-*.zip`: `libtorch/{lib,include,build-version,build-hash}`.
    LibtorchZip,
    /// A `torch-*.whl`: `torch/{lib,include,version.py}`.
    Wheel,
}

impl Layout {
    pub fn prefix(self) -> &'static str {
        match self {
            Layout::LibtorchZip => "libtorch/",
            Layout::Wheel => "torch/",
        }
    }
}

/// A PyPI wheel (NVIDIA's CUDA libraries): which members the pack needs from it.
#[derive(Debug, Clone, Copy)]
pub struct Wheel {
    pub name: &'static str,
    pub version: &'static str,
    pub upstream: Upstream,
    /// SPDX id or `LicenseRef-*`, as shown to the user before the download.
    pub license: &'static str,
    /// Member paths in the wheel; each lands in the pack's `lib/` under its file name.
    pub libs: &'static [&'static str],
    /// The licence text(s) in the wheel; each lands in `licenses/<name>/`.
    pub license_files: &'static [&'static str],
}

/// One pack variant for one target.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub variant: &'static str,
    /// Rust target triples this spec builds for.
    pub targets: &'static [&'static str],
    pub libtorch: Upstream,
    pub layout: Layout,
    /// File names (or `prefix*` globs) under libtorch's `lib/` that the pack keeps.
    /// Everything else (headers, cmake files, test libraries, unused GPU libraries,
    /// import libraries) is left out. Every entry must match something.
    pub keep: &'static [&'static str],
    /// Sub-directories of `lib/` with per-GPU-architecture kernel data (ROCm), kept
    /// only for [`Spec::gpu_archs`] plus the arch-independent files.
    pub arch_dirs: &'static [&'static str],
    /// GPU architectures the kernel data is trimmed to.
    pub gpu_archs: &'static [&'static str],
    /// NVIDIA libraries fetched from PyPI (or bundled with `--bundle-external`).
    pub wheels: &'static [Wheel],
    /// Shared libraries the pack may need from the host (beyond the C/C++ runtime).
    pub system_libs: &'static [&'static str],
    /// Minimum NVIDIA driver.
    pub nvidia_driver: Option<&'static str>,
    /// Libraries libtorch links (`NEEDED`) but OCR never calls: replaced by generated
    /// stubs (same SONAME and symbol versions; every function aborts with a message),
    /// so the real ones are neither shipped nor downloaded (Linux).
    pub stubs: &'static [&'static str],
    /// Licence texts kept in the repository (relative to the workspace root) for
    /// libraries the upstream archive ships without them; copied into `licenses/<last
    /// path component>/`.
    pub license_dir: Option<&'static str>,
    /// Data files kept in the repository (relative to the workspace root), copied into
    /// the pack's `share/` (ROCm: libdrm's `amdgpu.ids`, which bunko-torch points
    /// `AMDGPU_ASIC_ID_TABLE_PATHS` at).
    pub share_dir: Option<&'static str>,
}

/// glibc + libstdc++ + libgcc: on every Linux host the full build runs on.
pub const LINUX_BASE_LIBS: &[&str] = &[
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "librt.so.1",
    "libpthread.so.0",
    "libutil.so.1",
    "libstdc++.so.6",
    "libgcc_s.so.1",
    "ld-linux-x86-64.so.2",
    "ld-linux-aarch64.so.1",
];

/// The PyTorch licence texts (LICENSE + bundled third-party licences): the dist-info
/// of the CPU wheel of the same release (the libtorch zips ship none).
pub const TORCH_LICENSES_WHEEL: Upstream = Upstream {
    url: "https://download.pytorch.org/whl/cpu/torch-2.13.0%2Bcpu-cp312-cp312-manylinux_2_28_x86_64.whl",
    sha256: "4ca4a9394b0c771238a4f73590fdbbc4debad85ed0fa63d026ae1b085da7d6e2",
    size: 191_817_609,
};

const LINUX_X64: &[&str] = &["x86_64-unknown-linux-gnu"];

const CUDA_EULA: &str = "LicenseRef-NVIDIA-CUDA-EULA";

/// CUDA 13.0 libraries as torch 2.13.0+cu130 pins them (`cuda-toolkit==13.0.3` extras,
/// nvidia-cudnn-cu13 9.20.0.48, -cusparselt-cu13 0.8.1, -nccl-cu13 2.29.7,
/// -nvshmem-cu13 3.4.5; nvjitlink pinned to 13.0.88, the CUDA 13.0 release).
/// Only the libraries libtorch's CUDA half links (`NEEDED`) or loads at run time.
/// cuFile, NVSHMEM and nvJitLink are linked but never called by the recognizers
/// (measured: stubs that abort on any call ran hayai-nova and paddle-manga on an RTX
/// 4090), and the CUDA EULA shipped in their wheels does not list them as
/// redistributable: they are replaced by stubs ([`Spec::stubs`]).
const CU130_LINUX_WHEELS: &[Wheel] = &[
    Wheel {
        name: "nvidia-cuda-runtime",
        version: "13.0.96",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/2e/24/d1558f3b68b1d26e706813b1d10aa1d785e4698c425af8db8edc3dced472/nvidia_cuda_runtime-13.0.96-py3-none-manylinux2014_x86_64.manylinux_2_17_x86_64.whl",
            sha256: "7f82250d7782aa23b6cfe765ecc7db554bd3c2870c43f3d1821f1d18aebf0548",
            size: 2_243_632,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/lib/libcudart.so.13"],
        license_files: &["nvidia_cuda_runtime-13.0.96.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cublas",
        version: "13.1.1.3",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/3b/cd/154ca20c38269e05eff77c1464e6c1da89f50a6390b565e9d82e06bc11e1/nvidia_cublas-13.1.1.3-py3-none-manylinux_2_27_x86_64.whl",
            sha256: "37936a16db8fe4ac1f065c2139360608a543a09275cb1a1af612e08cfa065436",
            size: 423_138_758,
        },
        license: CUDA_EULA,
        libs: &[
            "nvidia/cu13/lib/libcublas.so.13",
            "nvidia/cu13/lib/libcublasLt.so.13",
        ],
        license_files: &["nvidia_cublas-13.1.1.3.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cuda-nvrtc",
        version: "13.0.88",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/c3/68/483a78f5e8f31b08fb1bb671559968c0ca3a065ac7acabfc7cee55214fd6/nvidia_cuda_nvrtc-13.0.88-py3-none-manylinux2010_x86_64.manylinux_2_12_x86_64.whl",
            sha256: "ad9b6d2ead2435f11cbb6868809d2adeeee302e9bb94bcf0539c7a40d80e8575",
            size: 90_215_200,
        },
        license: CUDA_EULA,
        libs: &[
            "nvidia/cu13/lib/libnvrtc.so.13",
            "nvidia/cu13/lib/libnvrtc-builtins.so.13.0",
        ],
        license_files: &["nvidia_cuda_nvrtc-13.0.88.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cuda-cupti",
        version: "13.0.85",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/33/6d/737d164b4837a9bbd202f5ae3078975f0525a55730fe871d8ed4e3b952b0/nvidia_cuda_cupti-13.0.85-py3-none-manylinux_2_25_x86_64.whl",
            sha256: "4eb01c08e859bf924d222250d2e8f8b8ff6d3db4721288cf35d14252a4d933c8",
            size: 10_715_597,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/lib/libcupti.so.13"],
        license_files: &["nvidia_cuda_cupti-13.0.85.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cufft",
        version: "12.0.0.61",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/a8/2f/7b57e29836ea8714f81e9898409196f47d772d5ddedddf1592eadb8ab743/nvidia_cufft-12.0.0.61-py3-none-manylinux2014_x86_64.manylinux_2_17_x86_64.whl",
            sha256: "6c44f692dce8fd5ffd3e3df134b6cdb9c2f72d99cf40b62c32dde45eea9ddad3",
            size: 214_085_489,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/lib/libcufft.so.12"],
        license_files: &["nvidia_cufft-12.0.0.61.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-curand",
        version: "10.4.0.35",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/a5/9f/be0a41ca4a4917abf5cb9ae0daff1a6060cc5de950aec0396de9f3b52bc5/nvidia_curand-10.4.0.35-py3-none-manylinux_2_27_x86_64.whl",
            sha256: "1aee33a5da6e1db083fe2b90082def8915f30f3248d5896bcec36a579d941bfc",
            size: 59_544_258,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/lib/libcurand.so.10"],
        license_files: &["nvidia_curand-10.4.0.35.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cusparse",
        version: "12.6.3.3",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/fa/18/623c77619c31d62efd55302939756966f3ecc8d724a14dab2b75f1508850/nvidia_cusparse-12.6.3.3-py3-none-manylinux2014_x86_64.manylinux_2_17_x86_64.whl",
            sha256: "2b3c89c88d01ee0e477cb7f82ef60a11a4bcd57b6b87c33f789350b59759360b",
            size: 145_942_937,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/lib/libcusparse.so.12"],
        license_files: &["nvidia_cusparse-12.6.3.3.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cudnn-cu13",
        version: "9.20.0.48",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/6e/5e/edb9c0ae051602c3ccaffe424256463636d639e27d7f302dde9975ef9e7a/nvidia_cudnn_cu13-9.20.0.48-py3-none-manylinux_2_27_x86_64.whl",
            sha256: "0c45dd8eeb50b603f07995b1b300c62ffe6a1980482b82b3bcf94a4ca9d49304",
            size: 366_173_588,
        },
        license: "LicenseRef-NVIDIA-cuDNN-SLA",
        // libcudnn.so.9 is a dispatcher that dlopen()s the engine/op libraries on first
        // use. libtorch_cuda links it, but neither recognizer calls cuDNN (LD_DEBUG on
        // an RTX 4090: hayai-nova and paddle-manga, bf16, load none of the seven
        // sub-libraries), so only the dispatcher is kept (-490 MB).
        libs: &["nvidia/cudnn/lib/libcudnn.so.9"],
        license_files: &["nvidia_cudnn_cu13-9.20.0.48.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cusparselt-cu13",
        version: "0.8.1",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/34/7d/2661f2fb3ac4302f3a246f5fc030213ac60c1fe0bce84f9783dbd831dbb7/nvidia_cusparselt_cu13-0.8.1-py3-none-manylinux2014_x86_64.whl",
            sha256: "786ce87568c303fadb5afcc7102d454cd3040d75f6f8626f5db460d1871f4dd0",
            size: 170_148_586,
        },
        license: "LicenseRef-NVIDIA-cuSPARSELt-SLA",
        libs: &["nvidia/cusparselt/lib/libcusparseLt.so.0"],
        license_files: &["nvidia/cusparselt/LICENSE.txt"],
    },
    Wheel {
        name: "nvidia-nccl-cu13",
        version: "2.29.7",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/67/f4/58e4e91b6919367c7aafb8e36fce9aad1a3047e536bf7e2fd560927d3a4c/nvidia_nccl_cu13-2.29.7-py3-none-manylinux_2_18_x86_64.whl",
            sha256: "edd81538446786ec3b73972543e53bb43bcaf0bfc8ef76cb679fcc390ffe136d",
            size: 205_976_000,
        },
        license: "BSD-3-Clause",
        libs: &["nvidia/nccl/lib/libnccl.so.2"],
        license_files: &["nvidia_nccl_cu13-2.29.7.dist-info/licenses/License.txt"],
    },
];

/// The same CUDA 13.0 libraries for Windows (win_amd64 wheels). The Windows libtorch
/// zip bundles these DLLs itself; the pack takes only torch's own DLLs from it and the
/// NVIDIA ones from NVIDIA's wheels, as on Linux. cudart and cuRAND are linked
/// statically into torch_cuda.dll on Windows; NCCL, cuFile, NVSHMEM and cuSPARSELt are
/// not used there. CUPTI is 13.0.48: torch_cpu.dll imports `cupti64_2025.3.0.dll`.
const CU130_WINDOWS_WHEELS: &[Wheel] = &[
    // The AOTInductor model DLLs (`*.wrapper.pyd`) import cudart64_13.dll; nothing in
    // the pack itself does (torch_cuda.dll links the runtime statically), so the
    // closure check cannot see it. Without it every GPU package fails to load
    // (`WinError 126`, measured on pimax).
    Wheel {
        name: "nvidia-cuda-runtime",
        version: "13.0.96",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/b7/94/6b867483bec07da24ffa32736c79fabb94ef3a7af4d787a9d4a974868576/nvidia_cuda_runtime-13.0.96-py3-none-win_amd64.whl",
            sha256: "f79298c8a098cec150a597c8eba58ecdab96e3bdc4b9bc4f9983635031740492",
            size: 2_927_037,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/bin/x86_64/cudart64_13.dll"],
        license_files: &["nvidia_cuda_runtime-13.0.96.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cublas",
        version: "13.1.1.3",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/45/9e/2f562daf80eb8f7a685fb7bea4fda71f6048e4f359d6fdd1b6e70206cb2f/nvidia_cublas-13.1.1.3-py3-none-win_amd64.whl",
            sha256: "b6cdce694e47ff6aadf0a69df1cab6628d696f5ff56e8d16af50309d855fa20f",
            size: 404_358_158,
        },
        license: CUDA_EULA,
        libs: &[
            "nvidia/cu13/bin/x86_64/cublas64_13.dll",
            "nvidia/cu13/bin/x86_64/cublasLt64_13.dll",
        ],
        license_files: &["nvidia_cublas-13.1.1.3.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cuda-nvrtc",
        version: "13.0.88",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/4a/af/345fedb9f4c76c84ab4fa445b36bd4048a4d9db60e6bc76b4f913ff4b852/nvidia_cuda_nvrtc-13.0.88-py3-none-win_amd64.whl",
            sha256: "6bcd4e7f8e205cbe644f5a98f2f799bef9556fefc89dd786e79a16312ce49872",
            size: 76_807_835,
        },
        license: CUDA_EULA,
        libs: &[
            "nvidia/cu13/bin/x86_64/nvrtc64_130_0.dll",
            "nvidia/cu13/bin/x86_64/nvrtc-builtins64_130.dll",
        ],
        license_files: &["nvidia_cuda_nvrtc-13.0.88.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cuda-cupti",
        version: "13.0.48",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/7e/ec/a2c11d70bc7dce659c484f16a8b565cc3c533f1af21374b7287736ff08e8/nvidia_cuda_cupti-13.0.48-py3-none-win_amd64.whl",
            sha256: "c0f0266d5674afad541888d4383bd172b7f90ff6df62df83ef9f5431a3c2c3b1",
            size: 7_737_757,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/bin/x86_64/cupti64_2025.3.0.dll"],
        license_files: &["nvidia_cuda_cupti-13.0.48.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cufft",
        version: "12.0.0.61",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/85/b2/f8af21a2ed1beed337a6a02c5a28aeb85441f4d578ec3d529543c775ea4b/nvidia_cufft-12.0.0.61-py3-none-win_amd64.whl",
            sha256: "2abce5b39d2f5ae12730fb7e5db6696533e36c26e2d3e8fd1750bdd2853364eb",
            size: 213_342_123,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/bin/x86_64/cufft64_12.dll"],
        license_files: &["nvidia_cufft-12.0.0.61.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cusolver",
        version: "12.0.4.66",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/99/ef/332a0101260ca78a1daef046bf0b06199e8ed4dac1d2aa698289c358169c/nvidia_cusolver-12.0.4.66-py3-none-win_amd64.whl",
            sha256: "16515bd33a8e76bb54d024cfa068fa68d30e80fc34b9e1090813ea9362e0cb65",
            size: 193_551_444,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/bin/x86_64/cusolver64_12.dll"],
        license_files: &["nvidia_cusolver-12.0.4.66.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cusparse",
        version: "12.6.3.3",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/02/b0/b043d6f3480f102f885cf87fc3ffd3edcb5e23b855025a50e2ef4d059185/nvidia_cusparse-12.6.3.3-py3-none-win_amd64.whl",
            sha256: "cbcf42feb737bd7ec15b4c0a63e62351886bd3f975027b8815d7f720a2b5ea79",
            size: 143_783_033,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/bin/x86_64/cusparse64_12.dll"],
        license_files: &["nvidia_cusparse-12.6.3.3.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-nvjitlink",
        version: "13.0.88",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/e4/01/07530b0e37546231052e30234540289c42eaffa486f1a34a87fed340157b/nvidia_nvjitlink-13.0.88-py3-none-win_amd64.whl",
            sha256: "634e96e3da9ef845ae744097a1f289238ecf946ce0b82e93cdce14b9782e682f",
            size: 36_035_115,
        },
        license: CUDA_EULA,
        libs: &["nvidia/cu13/bin/x86_64/nvJitLink_130_0.dll"],
        license_files: &["nvidia_nvjitlink-13.0.88.dist-info/licenses/License.txt"],
    },
    Wheel {
        name: "nvidia-cudnn-cu13",
        version: "9.20.0.48",
        upstream: Upstream {
            url: "https://files.pythonhosted.org/packages/78/39/21507455b1bca8b5702a9e9fc6ce73735f216f558dac2c9ede58e4d456b8/nvidia_cudnn_cu13-9.20.0.48-py3-none-win_amd64.whl",
            sha256: "af8139732b99c0118be65ea5aac97f0d46018f8c552889e49d2fb0c6261a4a24",
            size: 350_712_614,
        },
        license: "LicenseRef-NVIDIA-cuDNN-SLA",
        // As on Linux: only the dispatcher (the recognizers never call cuDNN).
        libs: &["nvidia/cudnn/bin/cudnn64_9.dll"],
        license_files: &["nvidia_cudnn_cu13-9.20.0.48.dist-info/licenses/License.txt"],
    },
];

/// ROCm GPU architectures the ROCm pack keeps kernels for (TORCH-BACKEND.md scope:
/// RDNA2 gfx1030 (gfx1031/1032 run it with HSA_OVERRIDE_GFX_VERSION=10.3.0), RDNA3,
/// RDNA4).
pub const ROCM_ARCHS: &[&str] = &[
    "gfx1030", "gfx1100", "gfx1101", "gfx1102", "gfx1200", "gfx1201",
];

pub const SPECS: &[Spec] = &[
    Spec {
        variant: "cpu",
        targets: LINUX_X64,
        libtorch: Upstream {
            url: "https://download.pytorch.org/libtorch/cpu/libtorch-shared-with-deps-2.13.0%2Bcpu.zip",
            sha256: "edbf4cbed78433d803e90a65f1752e57783d164bce66c95c0872b2ab8f5c159e",
            size: 126_385_248,
        },
        layout: Layout::LibtorchZip,
        keep: &[
            "libc10.so",
            "libtorch.so",
            "libtorch_cpu.so",
            "libgomp.so.1",
        ],
        arch_dirs: &[],
        gpu_archs: &[],
        wheels: &[],
        system_libs: &[],
        nvidia_driver: None,
        stubs: &[],
        license_dir: None,
        share_dir: None,
    },
    Spec {
        variant: "cu130",
        targets: LINUX_X64,
        libtorch: Upstream {
            url: "https://download.pytorch.org/libtorch/cu130/libtorch-shared-with-deps-2.13.0%2Bcu130.zip",
            sha256: "945c5a3d946a28b387ad9dc9fddda7ba03e35fae1375b84ebff15df789436f82",
            size: 500_687_821,
        },
        layout: Layout::LibtorchZip,
        // libtorch_cuda_linalg.so (cuSOLVER, ~185 MB with its libraries) is only
        // dlopen()ed by torch.linalg on the GPU, which the recognizers never call.
        keep: &[
            "libc10.so",
            "libc10_cuda.so",
            "libcaffe2_nvrtc.so",
            "libgomp.so.1",
            "libtorch.so",
            "libtorch_cpu.so",
            "libtorch_cuda.so",
            "libtorch_nvshmem.so",
        ],
        arch_dirs: &[],
        gpu_archs: &[],
        wheels: CU130_LINUX_WHEELS,
        // libcuda.so.1 is the driver (the NVIDIA container toolkit mounts it); cuDNN
        // links zlib.
        system_libs: &["libcuda.so.1", "libz.so.1"],
        nvidia_driver: Some("580.65.06"),
        // libtorch_cuda -> libcufile (no symbol imported), libtorch_nvshmem ->
        // libnvshmem_host (19 functions), libcusparse -> libnvJitLink (8 functions).
        stubs: &[
            "libcufile.so.0",
            "libnvshmem_host.so.3",
            "libnvJitLink.so.13",
        ],
        license_dir: None,
        share_dir: None,
    },
    Spec {
        variant: "rocm7.1",
        targets: LINUX_X64,
        libtorch: Upstream {
            url: "https://download.pytorch.org/libtorch/rocm7.1/libtorch-shared-with-deps-2.13.0%2Brocm7.1.zip",
            sha256: "d40daccce594700356a86b2ce3f1f53d47bf0a3b2047db66d9512feb21da85d1",
            size: 5_763_186_289,
        },
        layout: Layout::LibtorchZip,
        keep: &[
            "libc10.so",
            "libc10_hip.so",
            "libcaffe2_nvrtc.so",
            "libgomp.so.1",
            "libtorch.so",
            "libtorch_cpu.so",
            "libtorch_hip.so",
            "libtorch_rocshmem.so",
            "libamdhip64.so",
            "libamd_comgr.so",
            "libhsa-runtime64.so",
            "libhsa-amd-aqlprofile64.so",
            "libhiprtc.so",
            "libhipblas.so",
            "libhipblaslt.so",
            "libhipfft.so",
            "libhiprand.so",
            "libhipsolver.so",
            "libhipsparse.so",
            "libhipsparselt.so",
            "libMIOpen.so",
            "libmagma.so",
            "librccl.so",
            "librocblas.so",
            "librocfft.so",
            "librocrand.so",
            "librocroller.so",
            "librocsolver.so",
            "librocsparse.so",
            "librocm-core.so",
            "librocm_smi64.so",
            "librocprofiler-register.so",
            "librocprofiler-sdk.so",
            "libroctx64.so",
            "libaotriton_v2.so.0.12.0",
            // MIT: kept so the HSA runtime gets the libdrm it was built with.
            "libdrm.so.2",
            "libdrm_amdgpu.so.1",
        ],
        arch_dirs: &["rocblas", "hipblaslt", "hipsparselt", "aotriton.images"],
        gpu_archs: ROCM_ARCHS,
        wheels: &[],
        // LGPL/GPL system libraries the zip bundles are taken from the host instead
        // (libnuma: numactl; libelf/libdw: elfutils), with the compression libraries.
        // libtorch_rocshmem dlopen()s the unversioned `libnuma.so` while loading and
        // exits when it is missing (Arch: numactl; Debian/Ubuntu: libnuma-dev).
        system_libs: &[
            "libnuma.so",
            "libnuma.so.1",
            "libelf.so.1",
            "libdw.so.1",
            "libz.so.1",
            "libzstd.so.1",
            "liblzma.so.5",
            "libbz2.so.1",
            "libatomic.so.1",
        ],
        nvidia_driver: None,
        stubs: &[],
        license_dir: Some("packaging/torch/licenses/rocm"),
        share_dir: Some("packaging/torch/share/rocm7.1"),
    },
    Spec {
        variant: "cpu",
        targets: &["x86_64-pc-windows-msvc"],
        libtorch: Upstream {
            url: "https://download.pytorch.org/libtorch/cpu/libtorch-win-shared-with-deps-2.13.0%2Bcpu.zip",
            sha256: "e1cd3950adc30c54c364eaaf44c2b0caf1eafee29a6e811c805e6fe1ce912759",
            size: 199_487_097,
        },
        layout: Layout::LibtorchZip,
        keep: &[
            "c10.dll",
            "torch.dll",
            "torch_cpu.dll",
            "libiomp5md.dll",
            "uv.dll",
        ],
        arch_dirs: &[],
        gpu_archs: &[],
        wheels: &[],
        system_libs: &[],
        nvidia_driver: None,
        stubs: &[],
        license_dir: None,
        share_dir: None,
    },
    Spec {
        variant: "cu130",
        targets: &["x86_64-pc-windows-msvc"],
        libtorch: Upstream {
            url: "https://download.pytorch.org/libtorch/cu130/libtorch-win-shared-with-deps-2.13.0%2Bcu130.zip",
            sha256: "be25958466f64d551221c3525f5d878200d3e3ef7c6eb36af6a282f6f6076dac",
            size: 3_786_552_970,
        },
        layout: Layout::LibtorchZip,
        keep: &[
            "c10.dll",
            "c10_cuda.dll",
            "caffe2_nvrtc.dll",
            "torch.dll",
            "torch_cpu.dll",
            "torch_cuda.dll",
            "libiomp5md.dll",
            "uv.dll",
        ],
        arch_dirs: &[],
        gpu_archs: &[],
        wheels: CU130_WINDOWS_WHEELS,
        system_libs: &[],
        nvidia_driver: Some("580.88"),
        stubs: &[],
        license_dir: None,
        share_dir: None,
    },
    Spec {
        variant: "cpu",
        targets: &["aarch64-apple-darwin"],
        libtorch: Upstream {
            url: "https://download.pytorch.org/libtorch/cpu/libtorch-macos-arm64-2.13.0.zip",
            sha256: "1e10c6c4dc2764150c9fb2ad28e1889191302734e7f939108e5d2f22f21a06f8",
            size: 88_779_559,
        },
        layout: Layout::LibtorchZip,
        keep: &[
            "libc10.dylib",
            "libtorch.dylib",
            "libtorch_cpu.dylib",
            "libomp.dylib",
        ],
        arch_dirs: &[],
        gpu_archs: &[],
        wheels: &[],
        system_libs: &[],
        nvidia_driver: None,
        stubs: &[],
        license_dir: None,
        share_dir: None,
    },
];

pub fn find(variant: &str, target: &str) -> Option<&'static Spec> {
    SPECS
        .iter()
        .find(|s| s.variant == variant && s.targets.contains(&target))
}

pub fn variants() -> Vec<String> {
    let mut v: Vec<String> = SPECS
        .iter()
        .flat_map(|s| {
            s.targets
                .iter()
                .map(move |t| format!("{} ({t})", s.variant))
        })
        .collect();
    v.sort();
    v
}

/// The GPU architecture a ROCm kernel-data file is for (`gfx1201` in
/// `Kernels.so-000-gfx1201.hsaco`), if its name says.
pub fn file_arch(name: &str) -> Option<&str> {
    let i = name.find("gfx")?;
    let rest = &name[i..];
    let end = rest[3..]
        .find(|c: char| !c.is_ascii_alphanumeric())
        .map_or(rest.len(), |e| e + 3);
    let arch = &rest[..end];
    (arch.len() > 3).then_some(arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_of_kernel_files() {
        assert_eq!(file_arch("Kernels.so-000-gfx1201.hsaco"), Some("gfx1201"));
        assert_eq!(
            file_arch("TensileLibrary_Type_HH_Contraction_l_Ailk_Bjlk_Cijk_Dijk_gfx90a.dat"),
            Some("gfx90a")
        );
        assert_eq!(
            file_arch("TensileLibrary_Type_SS_Contraction_l_Ailk_Bjlk_Cijk_Dijk_fallback.dat"),
            None
        );
        assert_eq!(file_arch("amd-gfx11xx/flash/x.aks2"), Some("gfx11xx"));
    }

    #[test]
    fn specs_are_well_formed() {
        for s in SPECS {
            assert!(s.libtorch.sha256.len() == 64, "{}", s.libtorch.url);
            assert!(!s.keep.is_empty());
            for w in s.wheels {
                assert_eq!(w.upstream.sha256.len(), 64, "{}", w.name);
                assert!(w.upstream.url.ends_with(".whl"));
                assert!(!w.libs.is_empty() && !w.license_files.is_empty());
            }
        }
        assert!(find("cu130", "x86_64-unknown-linux-gnu").is_some());
        assert!(find("cu130", "aarch64-apple-darwin").is_none());
        assert!(find("cu130", "x86_64-pc-windows-msvc").is_some());
        // ROCm ships its licence texts from the repository; the CUDA pack stubs the
        // libraries the EULA does not list as redistributable.
        let rocm = find("rocm7.1", "x86_64-unknown-linux-gnu").unwrap();
        let dir = crate::util::workspace_root().join(rocm.license_dir.unwrap());
        assert!(dir.join("SOURCES.md").is_file() && dir.join("rocblas/LICENSE.md").is_file());
        let share = crate::util::workspace_root().join(rocm.share_dir.unwrap());
        assert!(share.join("libdrm/amdgpu.ids").is_file());
        let cu = find("cu130", "x86_64-unknown-linux-gnu").unwrap();
        for lib in [
            "libcufile.so.0",
            "libnvshmem_host.so.3",
            "libnvJitLink.so.13",
        ] {
            assert!(cu.stubs.contains(&lib));
            assert!(
                !cu.wheels
                    .iter()
                    .any(|w| w.libs.iter().any(|m| m.ends_with(lib)))
            );
        }
        assert!(TORCH_LICENSES_WHEEL.url.ends_with(".whl"));
    }
}
