// The NVENC bridge again, against API 13.0's header (this folder): every entry
// point suffixed `_v13`. Only opened where the driver offers 13.0 (570 on).
#define RDP_NVENC_V13
#include "nvEncodeAPI.h"
#include "../rdp_nvenc.c"
