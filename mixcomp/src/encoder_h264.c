// H.264 编码(openh264):屏幕内容实时档,BGRA 输入转 I420,输出 Annex-B。
// 订阅者协商 h264 且编译期探测到 openh264 时,由 stream.c 每窗口创建。
#include "mixcomp.h"

#ifdef HAVE_OPENH264

#include <stdlib.h>
#include <string.h>
#include <wels/codec_api.h>

struct h264_encoder {
	ISVCEncoder *svc;
	int width, height;
	// I420 平面(BGRA 转换的落点,encoder 直读)
	unsigned char *y, *u, *v;
};

// BGRA(行距 stride,源尺寸 src_w×src_h)→ I420,BT.601 limited。
// 编码器尺寸为源尺寸向上 2 对齐,越界处复制边缘像素。
static void bgra_to_i420(struct h264_encoder *e, const unsigned char *bgra,
		int stride, int src_w, int src_h) {
	const int w = e->width, h = e->height;
	const int cw = (w + 1) / 2;
	if (src_w > w) src_w = w;
	if (src_h > h) src_h = h;
	for (int y = 0; y < h; y++) {
		const int sy = y < src_h ? y : src_h - 1;
		const unsigned char *row = bgra + (size_t)sy * stride;
		unsigned char *yrow = e->y + (size_t)y * w;
		for (int x = 0; x < w; x++) {
			const int sx = x < src_w ? x : src_w - 1;
			const int b = row[sx * 4], g = row[sx * 4 + 1], r = row[sx * 4 + 2];
			yrow[x] = (unsigned char)((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
			if ((y & 1) == 0 && (x & 1) == 0) {
				const int u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
				const int v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
				e->u[(size_t)(y / 2) * cw + x / 2] = (unsigned char)u;
				e->v[(size_t)(y / 2) * cw + x / 2] = (unsigned char)v;
			}
		}
	}
}

struct h264_encoder *h264_open(int width, int height, int fps) {
	// openh264 要求 2 对齐
	int w = (width + 1) & ~1, h = (height + 1) & ~1;
	if (w <= 0 || h <= 0 || fps <= 0) {
		return NULL;
	}
	struct h264_encoder *e = calloc(1, sizeof(*e));
	if (e == NULL) {
		return NULL;
	}
	e->width = w;
	e->height = h;
	e->y = malloc((size_t)w * h);
	e->u = malloc((size_t)((w + 1) / 2) * ((h + 1) / 2));
	e->v = malloc((size_t)((w + 1) / 2) * ((h + 1) / 2));
	if (e->y == NULL || e->u == NULL || e->v == NULL ||
			WelsCreateSVCEncoder(&e->svc) != 0) {
		h264_close(e);
		return NULL;
	}
	SEncParamExt param;
	memset(&param, 0, sizeof(param));
	(*e->svc)->GetDefaultParams(e->svc, &param);
	param.iUsageType = SCREEN_CONTENT_REAL_TIME;
	param.iPicWidth = w;
	param.iPicHeight = h;
	param.fMaxFrameRate = (float)fps;
	param.iTargetBitrate = width * height * fps / 500; // ≈2Mbps @1080p30
	if (param.iTargetBitrate < 512 * 1024) {
		param.iTargetBitrate = 512 * 1024;
	}
	param.iRCMode = RC_BITRATE_MODE;
	param.bEnableFrameSkip = false;
	param.uiIntraPeriod = (unsigned int)fps * 2; // 2 秒周期 IDR
	param.iSpatialLayerNum = 1;
	param.sSpatialLayers[0].iVideoWidth = w;
	param.sSpatialLayers[0].iVideoHeight = h;
	param.sSpatialLayers[0].fFrameRate = (float)fps;
	param.sSpatialLayers[0].iSpatialBitrate = param.iTargetBitrate;
	if ((*e->svc)->InitializeExt(e->svc, &param) != 0) {
		h264_close(e);
		return NULL;
	}
	return e;
}

void h264_close(struct h264_encoder *e) {
	if (e == NULL) {
		return;
	}
	if (e->svc != NULL) {
		(*e->svc)->Uninitialize(e->svc);
		WelsDestroySVCEncoder(e->svc);
	}
	free(e->y);
	free(e->u);
	free(e->v);
	free(e);
}

unsigned char *h264_encode(struct h264_encoder *e, const unsigned char *bgra,
		int stride, int src_w, int src_h, int force_idr,
		unsigned long *out_size, int *out_keyframe) {
	*out_size = 0;
	*out_keyframe = 0;
	if (force_idr) {
		(*e->svc)->ForceIntraFrame(e->svc, true);
	}
	bgra_to_i420(e, bgra, stride, src_w, src_h);
	SSourcePicture pic;
	memset(&pic, 0, sizeof(pic));
	pic.iPicWidth = e->width;
	pic.iPicHeight = e->height;
	pic.iColorFormat = videoFormatI420;
	pic.iStride[0] = e->width;
	pic.iStride[1] = pic.iStride[2] = (e->width + 1) / 2;
	pic.pData[0] = e->y;
	pic.pData[1] = e->u;
	pic.pData[2] = e->v;
	SFrameBSInfo info;
	memset(&info, 0, sizeof(info));
	if ((*e->svc)->EncodeFrame(e->svc, &pic, &info) != cmResultSuccess ||
			info.eFrameType == videoFrameTypeSkip) {
		return NULL;
	}
	unsigned long total = 0;
	for (int i = 0; i < info.iLayerNum; i++) {
		for (int j = 0; j < info.sLayerInfo[i].iNalCount; j++) {
			total += (unsigned long)info.sLayerInfo[i].pNalLengthInByte[j];
		}
	}
	if (total == 0) {
		return NULL;
	}
	unsigned char *out = malloc(total);
	if (out == NULL) {
		return NULL;
	}
	unsigned long off = 0;
	for (int i = 0; i < info.iLayerNum; i++) {
		const unsigned char *bs = info.sLayerInfo[i].pBsBuf;
		for (int j = 0; j < info.sLayerInfo[i].iNalCount; j++) {
			const int n = info.sLayerInfo[i].pNalLengthInByte[j];
			memcpy(out + off, bs, (size_t)n);
			off += (unsigned long)n;
			bs += n;
		}
	}
	*out_size = total;
	*out_keyframe = info.eFrameType == videoFrameTypeIDR;
	return out;
}

#endif // HAVE_OPENH264
