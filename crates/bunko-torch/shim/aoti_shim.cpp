// Thin C ABI over torch::inductor::AOTIModelPackageLoader for the Rust (tch) side.
// Tensors cross as at::Tensor* (tch's C_tensor); outputs are heap-allocated for tch to own.
// No platform calls here: loading the GPU half of libtorch is done from Rust (libloading),
// so this file builds unchanged on Linux, Windows and macOS.
#include <torch/csrc/inductor/aoti_package/model_package_loader.h>
#include <torch/csrc/inductor/aoti_runner/model_container_runner.h>
#include <ATen/ATen.h>
#include <ATen/DynamicLibrary.h>
#include <cstdlib>
#include <cstring>
#include <string>
#include <unordered_map>
#include <vector>

extern "C" {

static char* dup_err(const std::string& s) {
  char* p = (char*)malloc(s.size() + 1);
  if (p) memcpy(p, s.c_str(), s.size() + 1);
  return p;
}

void bt_aoti_free_err(char* e) { free(e); }

// `path` is a .pt2 zip (extracted to the temp directory by libtorch) or that zip
// unpacked into a directory (loaded in place). device_index -1 = the CPU.
void* bt_aoti_load(const char* path, int device_index, char** err) {
  try {
    return new torch::inductor::AOTIModelPackageLoader(path, "model", false, 1,
                                                       (c10::DeviceIndex)device_index);
  } catch (const std::exception& e) {
    *err = dup_err(e.what());
    return nullptr;
  } catch (...) {
    *err = dup_err("unknown C++ exception while loading");
    return nullptr;
  }
}

void bt_aoti_drop(void* h) { delete (torch::inductor::AOTIModelPackageLoader*)h; }

// The package metadata value of `key` (malloc'd; free with bt_aoti_free_err), or null.
char* bt_aoti_metadata(void* h, const char* key) {
  try {
    auto meta = ((torch::inductor::AOTIModelPackageLoader*)h)->get_metadata();
    auto it = meta.find(key);
    if (it == meta.end()) return nullptr;
    return dup_err(it->second);
  } catch (...) {
    return nullptr;
  }
}

// The constant FQNs of the package, '\n'-separated (malloc'd), or null on error.
char* bt_aoti_constant_fqns(void* h, char** err) {
  try {
    auto names = ((torch::inductor::AOTIModelPackageLoader*)h)->get_constant_fqns();
    std::string out;
    for (size_t i = 0; i < names.size(); i++) {
      if (i) out.push_back('\n');
      out += names[i];
    }
    return dup_err(out);
  } catch (const std::exception& e) {
    *err = dup_err(e.what());
    return nullptr;
  }
}

// The runner's container handle and model library (protected members; read through a
// pointer to member named from a derived class, which needs no instance of it).
struct RunnerPeek : torch::inductor::AOTIModelContainerRunner {
  static AOTInductorModelContainerHandle container(torch::inductor::AOTIModelContainerRunner* r) {
    return r->*(&RunnerPeek::container_handle_);
  }
  static at::DynamicLibrary* library(torch::inductor::AOTIModelContainerRunner* r) {
    return (r->*(&RunnerPeek::model_so_)).get();
  }
};

// Binds the package's constants to caller-owned tensors, given by original FQN (no copy:
// the caller keeps them alive as long as the package). Returns 0, or -1 with `err`.
//
// Through the model library's C-ABI `AOTInductorModelContainerUpdateUserManagedConstant
// BufferPairs` (name -> AtenTensorHandle pairs, keyed by the container's internal constant
// names), never libtorch's `load_constants`: that hands the library a
// `std::unordered_map*`, which a Windows package (MinGW/libc++, cross-built) reads with the
// wrong STL layout. One path on every platform; the same tensors are bound either way.
int bt_aoti_load_constants(void* h, const char** fqns, at::Tensor** tensors, int n, char** err) {
  try {
    auto* runner = ((torch::inductor::AOTIModelPackageLoader*)h)->get_runner();
    if (!runner) {
      *err = dup_err("the package has no runner");
      return -1;
    }
    std::unordered_map<std::string, at::Tensor*> by_fqn;
    by_fqn.reserve(n);
    for (int i = 0; i < n; i++) by_fqn.emplace(fqns[i], tensors[i]);
    const auto names = runner->getConstantNamesToOriginalFQNs();
    std::vector<AOTInductorConstantMapEntry> pairs;
    pairs.reserve(names.size());
    for (const auto& kv : names) {
      auto it = by_fqn.find(kv.second);
      if (it == by_fqn.end()) {
        *err = dup_err("no tensor for constant " + kv.second + " (" + kv.first + ")");
        return -1;
      }
      // `names` outlives the call: its keys back the pairs' names.
      pairs.push_back({kv.first.c_str(), reinterpret_cast<AtenTensorHandle>(it->second)});
    }
    auto* lib = RunnerPeek::library(runner);
    auto container = RunnerPeek::container(runner);
    if (!lib || !container) {
      *err = dup_err("the package's model library is not loaded");
      return -1;
    }
    using PairsFn = decltype(&AOTInductorModelContainerUpdateUserManagedConstantBufferPairs);
    auto bind = reinterpret_cast<PairsFn>(
        lib->sym("AOTInductorModelContainerUpdateUserManagedConstantBufferPairs"));
    AOTIRuntimeError rc = bind(container, pairs.data(), pairs.size(), false, true);
    if (rc != AOTI_RUNTIME_SUCCESS) {
      std::string msg = "binding " + std::to_string(pairs.size()) + " constants failed";
      try {
        using LastErrorFn = decltype(&AOTInductorGetLastError);
        auto last = reinterpret_cast<LastErrorFn>(lib->sym("AOTInductorGetLastError"));
        const char* m = nullptr;
        if (last(&m) == AOTI_RUNTIME_SUCCESS && m && *m) msg += std::string(": ") + m;
      } catch (...) {
      }
      *err = dup_err(msg);
      return -1;
    }
    return 0;
  } catch (const std::exception& e) {
    *err = dup_err(e.what());
    return -1;
  } catch (...) {
    *err = dup_err("unknown C++ exception while binding constants");
    return -1;
  }
}

// Returns the number of outputs written to `outs` (<= max_out), -1 on error.
int bt_aoti_run(void* h, at::Tensor** ins, int n_in, at::Tensor** outs, int max_out, char** err) {
  try {
    std::vector<at::Tensor> in;
    in.reserve(n_in);
    for (int i = 0; i < n_in; i++) in.push_back(*ins[i]);
    auto out = ((torch::inductor::AOTIModelPackageLoader*)h)->run(in);
    if ((int)out.size() > max_out) {
      *err = dup_err("too many outputs");
      return -1;
    }
    for (size_t i = 0; i < out.size(); i++) outs[i] = new at::Tensor(std::move(out[i]));
    return (int)out.size();
  } catch (const std::exception& e) {
    *err = dup_err(e.what());
    return -1;
  } catch (...) {
    *err = dup_err("unknown C++ exception while running");
    return -1;
  }
}
}
