#!/bin/bash
# build the bench against an ncnn install: build.sh <ncnn install prefix> <out binary> [extra flags]
set -e
here=$(cd "$(dirname "$0")" && pwd)
N=$1; OUT=$2; shift 2
g++ -std=c++17 -O2 -g "$@" -I"$N/include/ncnn" "$here/bunko_ncnn.cpp" "$here/bench/bench.cpp" -I"$here" -o "$OUT" \
  -L"$N/lib" -lncnn -lglslang -lSPIRV -lMachineIndependent -lGenericCodeGen -lOSDependent -lglslang-default-resource-limits \
  -fopenmp -lpthread -ldl
