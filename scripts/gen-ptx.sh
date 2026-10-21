#!/bin/sh
# Regenerates crates/host/src/color.ptx from color.cu. PTX for compute_52 is JIT-compiled
# by the driver for any newer GPU, so servers need no CUDA toolkit (no NVRTC).
set -eu
cd "$(dirname "$0")/../crates/host/src"
nvcc -arch=compute_52 -ptx -O3 -Wno-deprecated-gpu-targets -o color.ptx color.cu
