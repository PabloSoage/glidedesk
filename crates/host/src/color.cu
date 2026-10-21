// RGB → YUV for the RDP H.264 codecs (see color.rs). After editing, regenerate the PTX the
// host embeds with scripts/gen-ptx.sh (needs nvcc; the server itself only needs the driver).

__device__ __forceinline__ unsigned char clamp8(int v) {
    return (unsigned char)(v < 0 ? 0 : (v > 255 ? 255 : v));
}

// Full-range BT.709 (integer coefficients as in FreeRDP's primitives).
__device__ __forceinline__ void yuv(const unsigned char* p, int order, int* y, int* u, int* v) {
    int r, g, b;
    if (order == 0) { b = p[0]; g = p[1]; r = p[2]; } else { r = p[0]; g = p[1]; b = p[2]; }
    *y = clamp8((54 * r + 183 * g + 18 * b) >> 8);
    *u = clamp8(((-29 * r - 99 * g + 128 * b) >> 8) + 128);
    *v = clamp8(((128 * r - 116 * g - 12 * b) >> 8) + 128);
}

// src: 32-bit pixels, src_w × src_h, src_pitch bytes per row; order 0 = BGRX, 1 = RGBA.
// main/aux: NV12, w × h (multiples of 16), pitch bytes per row; aux may be 0.
extern "C" __global__ void rgb_to_avc(
    const unsigned char* src, int src_pitch, int src_w, int src_h, int order,
    unsigned char* main, unsigned char* aux, int pitch, int w, int h)
{
    int bx = blockIdx.x * blockDim.x + threadIdx.x;   // block column (2 pixels)
    int by = blockIdx.y * blockDim.y + threadIdx.y;   // block row (2 pixels)
    if (bx * 2 >= w || by * 2 >= h) return;
    int x = bx * 2, y = by * 2;

    int Y[4], U[4], V[4];
    for (int i = 0; i < 4; i++) {
        int sx = min(x + (i & 1), src_w - 1);
        int sy = min(y + (i >> 1), src_h - 1);
        yuv(src + (size_t)sy * src_pitch + sx * 4, order, &Y[i], &U[i], &V[i]);
    }
    // i: 0 = (x, y), 1 = (x+1, y), 2 = (x, y+1), 3 = (x+1, y+1)

    unsigned char* my = main;
    unsigned char* muv = main + (size_t)pitch * h;
    my[(size_t)y * pitch + x] = Y[0];
    my[(size_t)y * pitch + x + 1] = Y[1];
    my[(size_t)(y + 1) * pitch + x] = Y[2];
    my[(size_t)(y + 1) * pitch + x + 1] = Y[3];
    muv[(size_t)by * pitch + 2 * bx] = (U[0] + U[1] + U[2] + U[3]) / 4;
    muv[(size_t)by * pitch + 2 * bx + 1] = (V[0] + V[1] + V[2] + V[3]) / 4;

    // The auxiliary layout is defined on the real frame size (as FreeRDP does): halves and
    // quarters of src_w, nothing written in the padding.
    if (aux == 0 || x >= src_w || y >= src_h) return;
    unsigned char* ay = aux;
    unsigned char* auv = aux + (size_t)pitch * h;
    int half = src_w / 2, quarter = src_w / 4;
    // B4/B5: U and V of the odd columns, every row, left and right halves of the luma plane.
    ay[(size_t)y * pitch + bx] = U[1];
    ay[(size_t)y * pitch + half + bx] = V[1];
    ay[(size_t)(y + 1) * pitch + bx] = U[3];
    ay[(size_t)(y + 1) * pitch + half + bx] = V[3];
    // B6/B7 (x % 4 == 0) into the U plane, B8/B9 (x % 4 == 2) into the V plane: U and V of
    // the even columns of the odd rows, left and right halves. NV12 interleaves U and V.
    int k = bx >> 1;
    int plane = bx & 1;   // 0 = U plane, 1 = V plane
    auv[(size_t)by * pitch + 2 * k + plane] = U[2];
    auv[(size_t)by * pitch + 2 * (quarter + k) + plane] = V[2];
}
