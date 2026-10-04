# Licences of the AMD ROCm libraries in the rocm7.1 OCR backend pack

The libtorch 2.13.0+rocm7.1 zip bundles these libraries but ships no licence text for them
(PACKAGING.md §8). The texts here come from the upstream sources of the bundled versions:
ROCm **7.1.1** (`librocm-core.so` reports `7.1.1.0-38`, RCCL 2.27.7, MIOpen 3.5.1, comgr from
llvm-project `roc-7.1.1`), aotriton **0.12** (`libaotriton_v2.so.0.12.0`; text from tag `0.12b`),
MAGMA (BSD-3-Clause `COPYRIGHT`, tag `v2.9.0`; the zip does not say which MAGMA it built) and
libdrm (MIT, per-file licence headers; libdrm has no top-level licence file; tag `libdrm-2.4.124`).
`xtask torch-pack` copies this directory into the pack as `licenses/rocm/`.

The pack also ships `share/libdrm/amdgpu.ids` (from `packaging/torch/share/rocm7.1/`): the
bundled libdrm_amdgpu looks for it at `/opt/amdgpu/share/libdrm/amdgpu.ids` (compiled in)
and otherwise walks the executable's directory (a stray "(null): No such file or
directory", +3 s, generic GPU names); bunko-torch points `AMDGPU_ASIC_ID_TABLE_PATHS` at the
pack's copy. The bundled library supports that variable (libdrm >= 2.4.130), so the table is
libdrm's `data/amdgpu.ids` at tag `libdrm-2.4.131` (sha256 `98ba44a40fb8da74…`).

| directory | covers | source | sha256 |
|---|---|---|---|
| `aotriton/LICENSE` | libaotriton_v2.so.0.12.0, aotriton.images/ | https://raw.githubusercontent.com/ROCm/aotriton/0.12b/LICENSE | `47fb6a076a04a405…` |
| `aqlprofile/LICENSE.md` | libhsa-amd-aqlprofile64.so | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/aqlprofile/LICENSE.md | `b185aaa652b0bf06…` |
| `clr/LICENSE.md` | libamdhip64.so, libhiprtc.so (HIP runtime implementation) | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/clr/LICENSE.md | `b185aaa652b0bf06…` |
| `comgr/LICENSE.txt` | libamd_comgr.so | https://raw.githubusercontent.com/ROCm/llvm-project/rocm-7.1.1/amd/comgr/LICENSE.txt | `fa6831fc9e8068c9…` |
| `comgr/NOTICES.txt` | libamd_comgr.so | https://raw.githubusercontent.com/ROCm/llvm-project/rocm-7.1.1/amd/comgr/NOTICES.txt | `7954660896f17c81…` |
| `composablekernel/LICENSE` | kernels inside libMIOpen.so / libhipblaslt.so (Composable Kernel) | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/composablekernel/LICENSE | `20f3b83dfda01bd1…` |
| `hip/LICENSE.md` | libamdhip64.so, libhiprtc.so (with clr) | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/hip/LICENSE.md | `b185aaa652b0bf06…` |
| `hipblas/LICENSE.md` | libhipblas.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/hipblas/LICENSE.md | `fec91d5fe42b9fef…` |
| `hipblaslt/LICENSE.md` | libhipblaslt.so, hipblaslt/library/ | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/hipblaslt/LICENSE.md | `b185aaa652b0bf06…` |
| `hipfft/LICENSE.md` | libhipfft.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/hipfft/LICENSE.md | `dc73cef4d65dbb7e…` |
| `hiprand/LICENSE.md` | libhiprand.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/hiprand/LICENSE.md | `b185aaa652b0bf06…` |
| `hipsolver/LICENSE.md` | libhipsolver.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/hipsolver/LICENSE.md | `67db69484a5bb6ec…` |
| `hipsparse/LICENSE.md` | libhipsparse.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/hipsparse/LICENSE.md | `b185aaa652b0bf06…` |
| `hipsparselt/LICENSE.md` | libhipsparselt.so, hipsparselt/library/ | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/hipsparselt/LICENSE.md | `b185aaa652b0bf06…` |
| `libdrm/amdgpu_device.c-header.txt` | libdrm.so.2, libdrm_amdgpu.so.1 | https://gitlab.freedesktop.org/mesa/drm/-/raw/libdrm-2.4.124/amdgpu/amdgpu_device.c (licence header) | `ba193aed89d1d9dc…` |
| `libdrm/xf86drm.c-header.txt` | libdrm.so.2, libdrm_amdgpu.so.1 | https://gitlab.freedesktop.org/mesa/drm/-/raw/libdrm-2.4.124/xf86drm.c (licence header) | `2878ef1787de3477…` |
| `libdrm/amdgpu_asic_id.c-header.txt` | `share/libdrm/amdgpu.ids` (the GPU name table libdrm_amdgpu reads; data, MIT like libdrm) | https://gitlab.freedesktop.org/mesa/libdrm/-/raw/libdrm-2.4.131/amdgpu/amdgpu_asic_id.c (licence header) | `ac448fd7704b7957…` |
| `magma/COPYRIGHT` | libmagma.so | https://raw.githubusercontent.com/icl-utk-edu/magma/v2.9.0/COPYRIGHT | `0c275a656f52627d…` |
| `miopen/LICENSE.md` | libMIOpen.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/miopen/LICENSE.md | `f64af72ceda1fcee…` |
| `rccl/LICENSE.txt` | librccl.so | https://raw.githubusercontent.com/ROCm/rccl/rocm-7.1.1/LICENSE.txt | `df3a5e9aee2c34d6…` |
| `rccl/NOTICES.txt` | librccl.so | https://raw.githubusercontent.com/ROCm/rccl/rocm-7.1.1/NOTICES.txt | `b3785a3a91bfd6d4…` |
| `rocblas/LICENSE.md` | librocblas.so, rocblas/library/ | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/rocblas/LICENSE.md | `b57f384b03b348ca…` |
| `rocfft/LICENSE.md` | librocfft.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/rocfft/LICENSE.md | `bff8e2d1d03f313c…` |
| `rocm-core/LICENSE.md` | librocm-core.so | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/rocm-core/LICENSE.md | `b185aaa652b0bf06…` |
| `rocm-smi-lib/LICENSE.md` | librocm_smi64.so | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/rocm-smi-lib/LICENSE.md | `b185aaa652b0bf06…` |
| `rocprofiler-register/LICENSE.md` | librocprofiler-register.so | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/rocprofiler-register/LICENSE.md | `b185aaa652b0bf06…` |
| `rocprofiler-sdk/LICENSE.md` | librocprofiler-sdk.so | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/rocprofiler-sdk/LICENSE.md | `b185aaa652b0bf06…` |
| `rocr-runtime/LICENSE.txt` | libhsa-runtime64.so | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/rocr-runtime/LICENSE.txt | `ffa5a77ce21419e2…` |
| `rocrand/LICENSE.md` | librocrand.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/rocrand/LICENSE.md | `b185aaa652b0bf06…` |
| `rocroller/LICENSE.md` | librocroller.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/shared/rocroller/LICENSE.md | `b185aaa652b0bf06…` |
| `rocsolver/LICENSE.md` | librocsolver.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/rocsolver/LICENSE.md | `a037bdde286c83bb…` |
| `rocsparse/LICENSE.md` | librocsparse.so | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/projects/rocsparse/LICENSE.md | `b185aaa652b0bf06…` |
| `roctracer/LICENSE.md` | libroctx64.so | https://raw.githubusercontent.com/ROCm/rocm-systems/rocm-7.1.1/projects/roctracer/LICENSE.md | `b185aaa652b0bf06…` |
| `tensile/LICENSE.md` | rocblas/library/ and hipblaslt/library/ kernel data (Tensile) | https://raw.githubusercontent.com/ROCm/rocm-libraries/rocm-7.1.1/shared/tensile/LICENSE.md | `b185aaa652b0bf06…` |

Not shipped (taken from the host, LGPL/GPL): libnuma, libelf, libdw; not needed: libtinfo.
PyTorch's own code in the pack (libtorch_hip, libtorch_rocshmem, libc10_hip, libcaffe2_nvrtc) is
covered by `licenses/pytorch/`.
