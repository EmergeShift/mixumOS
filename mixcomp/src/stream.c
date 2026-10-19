// stream.c —— 与 mixumd 的控制通道：JSON 控制消息 + JPEG 帧推送。
#include <errno.h>
#include <fcntl.h>
#include <json-c/json.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <drm_fourcc.h>
#include <jpeglib.h>
#include <png.h>

#include "mixcomp.h"

// 发送结果语义。AF_UNIX + 非阻塞下 send 可能部分写出，而记录流一旦
// 半途而废，后续字节全对不上界。因此：
//   零字节写出即阻塞 → 整条记录可安全丢弃（帧拥塞"实时性优先"）；
//   部分写出后阻塞   → poll 短等排空，等不出必须断线重连（定界已破）。
typedef enum {
	REC_SEND_OK = 0,
	REC_SEND_AGAIN,
	REC_SEND_FATAL,
} rec_send_rc;

static rec_send_rc rec_send_raw(int fd, const uint8_t *buf, size_t total) {
	size_t off = 0;
	while (off < total) {
		ssize_t n = send(fd, buf + off, total - off, MSG_NOSIGNAL);
		if (n > 0) {
			off += (size_t)n;
			continue;
		}
		if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
			struct pollfd p = { .fd = fd, .events = POLLOUT };
			if (poll(&p, 1, 200) > 0) {
				continue;
			}
			return off == 0 ? REC_SEND_AGAIN : REC_SEND_FATAL;
		}
		return REC_SEND_FATAL;
	}
	return REC_SEND_OK;
}

// stream_mark_dead —— 连接死亡统一收尾：关 fd、清接收缓冲（旧连接的
// 半条记录绝不能带进重连后的新流）。
static void stream_mark_dead(struct mixcomp_server *s) {
	s->sock_ready = false;
	if (s->sock_fd >= 0) {
		close(s->sock_fd);
		s->sock_fd = -1;
	}
	s->rx_have = 0;
	wlr_log(WLR_ERROR, "mixumd 控制通道断开，等待重连");
}

// 记录封装：4 字节小端长度 + 1 字节类型 + 负载（与 mixumd display.Broker 对齐）。
static rec_send_rc rec_send(int fd, uint8_t type, const void *payload, size_t len) {
	if (fd < 0 || len > (16u << 20)) {
		return REC_SEND_FATAL;
	}
	uint8_t hdr[5] = {
		(uint8_t)(len & 0xff), (uint8_t)((len >> 8) & 0xff),
		(uint8_t)((len >> 16) & 0xff), (uint8_t)((len >> 24) & 0xff), type,
	};
	size_t total = sizeof(hdr) + len;
	uint8_t *buf = malloc(total);
	if (buf == NULL) {
		return REC_SEND_FATAL;
	}
	memcpy(buf, hdr, sizeof(hdr));
	memcpy(buf + sizeof(hdr), payload, len);
	rec_send_rc rc = rec_send_raw(fd, buf, total);
	free(buf);
	return rc;
}

bool stream_connect(struct mixcomp_server *s) {
	int fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (fd < 0) {
		return false;
	}
	struct sockaddr_un addr = {0};
	addr.sun_family = AF_UNIX;
	snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", s->sock_path);
	if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
		close(fd);
		return false;
	}
	int flags = fcntl(fd, F_GETFL, 0);
	fcntl(fd, F_SETFL, flags | O_NONBLOCK);
	s->sock_fd = fd;
	s->rx_have = 0;
	s->sock_ready = true;
	wlr_log(WLR_INFO, "已连接 mixumd 控制通道: %s", s->sock_path);
	return true;
}

// stream_flush —— 控制通道断开时轮询重连（由主循环定时调用）。
void stream_flush(struct mixcomp_server *s) {
	if (s->sock_ready) {
		return;
	}
	stream_connect(s);
}

void stream_send_window(struct mixcomp_server *s, struct mixcomp_window *w,
		const char *type) {
	if (!s->sock_ready) {
		return;
	}
	json_object *root = json_object_new_object();
	json_object_object_add(root, "type", json_object_new_string(type));
	json_object_object_add(root, "id", json_object_new_int(w->id));
	json_object_object_add(root, "appId", json_object_new_string(w->app_id));
	json_object_object_add(root, "title", json_object_new_string(w->title));
	json_object_object_add(root, "width", json_object_new_int(w->width));
	json_object_object_add(root, "height", json_object_new_int(w->height));
	const char *str = json_object_to_json_string(root);
	rec_send_rc rc = rec_send(s->sock_fd, REC_CONTROL, str, strlen(str));
	if (rc == REC_SEND_FATAL) {
		stream_mark_dead(s);
	} else if (rc == REC_SEND_AGAIN) {
		wlr_log(WLR_ERROR, "窗口 %u 控制消息 %s 拥塞丢弃", w->id, type);
	}
	json_object_put(root);
}

// stream.info —— 编码能力回告:mixumd 据此统一订阅者编码并广播 window 事件。
static void stream_send_info(struct mixcomp_server *s, int id,
		const char *codec) {
	if (!s->sock_ready) {
		return;
	}
	json_object *root = json_object_new_object();
	json_object_object_add(root, "type", json_object_new_string("stream.info"));
	json_object_object_add(root, "id", json_object_new_int(id));
	json_object_object_add(root, "codec", json_object_new_string(codec));
	const char *str = json_object_to_json_string(root);
	rec_send_rc rc = rec_send(s->sock_fd, REC_CONTROL, str, strlen(str));
	if (rc == REC_SEND_FATAL) {
		stream_mark_dead(s);
	}
	json_object_put(root);
}

// encode_jpeg —— 按 DRM 格式选择 libjpeg 扩展色彩空间。
static unsigned char *encode_jpeg(const unsigned char *pix, int stride,
		int width, int height, uint32_t drm_format, int quality,
		unsigned long *out_size) {
	J_COLOR_SPACE cs;
	switch (drm_format) {
	case DRM_FORMAT_ARGB8888:
	case DRM_FORMAT_XRGB8888:
		cs = JCS_EXT_BGRA; // 内存序 B,G,R,A
		break;
	case DRM_FORMAT_ABGR8888:
	case DRM_FORMAT_XBGR8888:
		cs = JCS_EXT_RGBA; // 内存序 R,G,B,A
		break;
	default:
		// 其余格式按 BGRA 容错（pixman 读回最常见的即 ARGB8888）。
		cs = JCS_EXT_BGRA;
		break;
	}
	struct jpeg_compress_struct cinfo;
	struct jpeg_error_mgr jerr;
	cinfo.err = jpeg_std_error(&jerr);
	jpeg_create_compress(&cinfo);
	unsigned char *out = NULL;
	*out_size = 0;
	jpeg_mem_dest(&cinfo, &out, out_size);
	cinfo.image_width = width;
	cinfo.image_height = height;
	cinfo.input_components = 4;
	cinfo.in_color_space = cs;
	jpeg_set_defaults(&cinfo);
	jpeg_set_quality(&cinfo, quality, TRUE);
	jpeg_start_compress(&cinfo, TRUE);
	while (cinfo.next_scanline < cinfo.image_height) {
		JSAMPROW row = (JSAMPROW)(pix + (size_t)cinfo.next_scanline * stride);
		jpeg_write_scanlines(&cinfo, &row, 1);
	}
	jpeg_finish_compress(&cinfo);
	jpeg_destroy_compress(&cinfo);
	return out;
}


// ---- PNG + damage（codec=png） -----------------------------------------
// 编码一块紧密 RGBA/BGRA 像素。libpng 简化 API：先问长度再写。
static unsigned char *encode_png(const unsigned char *pix, int stride,
		int width, int height, uint32_t drm_format, unsigned long *out_size) {
	png_image image;
	memset(&image, 0, sizeof(image));
	image.version = PNG_IMAGE_VERSION;
	image.width = (png_uint_32)width;
	image.height = (png_uint_32)height;
	switch (drm_format) {
	case DRM_FORMAT_ABGR8888:
	case DRM_FORMAT_XBGR8888:
		image.format = PNG_FORMAT_RGBA;
		break;
	default:
		image.format = PNG_FORMAT_BGRA;
		break;
	}
	png_alloc_size_t size = 0;
	if (!png_image_write_to_memory(&image, NULL, &size, 0, pix, stride, NULL)) {
		return NULL;
	}
	unsigned char *out = malloc(size ? size : 1);
	if (out == NULL) {
		return NULL;
	}
	if (!png_image_write_to_memory(&image, out, &size, 0, pix, stride, NULL)) {
		free(out);
		return NULL;
	}
	*out_size = (unsigned long)size;
	return out;
}

// 比较当前帧与上一帧，返回脏区包围盒（无变化返回 false）。
static bool diff_bbox(const uint8_t *cur, const uint8_t *prev, int width,
		int height, int *out_x, int *out_y, int *out_w, int *out_h) {
	int min_x = width, min_y = height, max_x = -1, max_y = -1;
	for (int y = 0; y < height; y++) {
		const uint32_t *a = (const uint32_t *)(cur + (size_t)y * width * 4);
		const uint32_t *b = (const uint32_t *)(prev + (size_t)y * width * 4);
		if (memcmp(a, b, (size_t)width * 4) == 0) {
			continue;
		}
		int first = -1, last = -1;
		for (int x = 0; x < width; x++) {
			if (a[x] != b[x]) {
				if (first < 0) {
					first = x;
				}
				last = x;
			}
		}
		if (first >= 0) {
			if (first < min_x) min_x = first;
			if (last > max_x) max_x = last;
			if (y < min_y) min_y = y;
			max_y = y;
		}
	}
	if (max_x < 0) {
		return false;
	}
	*out_x = min_x;
	*out_y = min_y;
	*out_w = max_x - min_x + 1;
	*out_h = max_y - min_y + 1;
	return true;
}

static void png_prev_update(struct mixcomp_window *w, const unsigned char *rgba,
		int width, int height, uint32_t drm_format) {
	size_t need = (size_t)width * (size_t)height * 4;
	if (w->prev_cap < need) {
		unsigned char *nb = realloc(w->prev, need);
		if (nb == NULL) {
			return;
		}
		w->prev = nb;
		w->prev_cap = need;
	}
	memcpy(w->prev, rgba, need);
	w->prev_w = width;
	w->prev_h = height;
	w->prev_fmt = drm_format;
	w->have_prev = true;
}

static void png_cursor_snapshot(struct mixcomp_window *w) {
	w->last_cursor_x = w->cursor_x;
	w->last_cursor_y = w->cursor_y;
	w->last_have_cursor = w->have_cursor;
}

static void send_png_record(struct mixcomp_server *s, struct mixcomp_window *w,
		uint32_t width, uint32_t height, uint16_t flags,
		const uint8_t *payload, size_t len) {
	if (!s->sock_ready || len > (16u << 20) - FRAME_HDR) {
		return;
	}
	uint8_t hdr[FRAME_HDR];
	hdr[0] = 0x01;
	uint32_t id = w->id, wd = width, ht = height, sz = (uint32_t)len;
	memcpy(hdr + 1, &id, 4);
	memcpy(hdr + 5, &wd, 4);
	memcpy(hdr + 9, &ht, 4);
	memcpy(hdr + 13, &sz, 4);
	if (w->have_cursor) {
		flags |= FRAME_FLAG_HASCURSOR;
	}
	memcpy(hdr + 17, &flags, 2);
	int16_t cx = (int16_t)w->cursor_x, cy = (int16_t)w->cursor_y;
	uint16_t cw = 16, ch = 16;
	memcpy(hdr + 19, &cx, 2);
	memcpy(hdr + 21, &cy, 2);
	memcpy(hdr + 23, &cw, 2);
	memcpy(hdr + 25, &ch, 2);

	size_t total = 5 + FRAME_HDR + len;
	uint8_t *rec = malloc(total);
	if (rec == NULL) {
		return;
	}
	uint32_t rlen = FRAME_HDR + (uint32_t)len;
	memcpy(rec, &rlen, 4);
	rec[4] = REC_VIDEO_PNG;
	memcpy(rec + 5, hdr, FRAME_HDR);
	if (len > 0) {
		memcpy(rec + 5 + FRAME_HDR, payload, len);
	}
	rec_send_rc rc = rec_send_raw(s->sock_fd, rec, total);
	if (rc == REC_SEND_FATAL) {
		stream_mark_dead(s);
	}
	free(rec);
}

// 首帧/尺寸变化发全量 PNG；否则只发脏区 PNG 补丁（无变化则不发）。
static void stream_send_png_frame(struct mixcomp_server *s,
		struct mixcomp_window *w, const unsigned char *rgba, int stride,
		int width, int height, uint32_t drm_format) {
	bool full = !w->have_prev || w->prev_w != width || w->prev_h != height ||
			w->prev_fmt != drm_format;
	if (!full) {
		int x = 0, y = 0, pw = 0, ph = 0;
		bool damaged = diff_bbox(rgba, w->prev, width, height, &x, &y, &pw, &ph);
		if (!damaged) {
			bool cursor_moved = w->have_cursor != w->last_have_cursor ||
					w->cursor_x != w->last_cursor_x ||
					w->cursor_y != w->last_cursor_y;
			if (cursor_moved) {
				uint8_t patch[PATCH_HDR] = {0};
				uint32_t fmt = PATCH_FORMAT_NONE;
				memcpy(patch + 16, &fmt, 4);
				send_png_record(s, w, (uint32_t)width, (uint32_t)height,
						FRAME_FLAG_PATCH, patch, sizeof(patch));
				png_cursor_snapshot(w);
			}
			return;
		}
		unsigned char *sub = malloc((size_t)pw * (size_t)ph * 4);
		if (sub == NULL) {
			return;
		}
		for (int r = 0; r < ph; r++) {
			memcpy(sub + (size_t)r * pw * 4,
					rgba + (size_t)(y + r) * stride + (size_t)x * 4,
					(size_t)pw * 4);
		}
		unsigned long png_size = 0;
		unsigned char *png = encode_png(sub, pw * 4, pw, ph, drm_format, &png_size);
		free(sub);
		if (png == NULL) {
			return;
		}
		size_t payload_len = PATCH_HDR + png_size;
		uint8_t *payload = malloc(payload_len);
		if (payload != NULL) {
			uint32_t ux = (uint32_t)x, uy = (uint32_t)y;
			uint32_t uw = (uint32_t)pw, uh = (uint32_t)ph;
			uint32_t fmt = PATCH_FORMAT_PNG;
			memcpy(payload, &ux, 4);
			memcpy(payload + 4, &uy, 4);
			memcpy(payload + 8, &uw, 4);
			memcpy(payload + 12, &uh, 4);
			memcpy(payload + 16, &fmt, 4);
			memcpy(payload + PATCH_HDR, png, png_size);
			send_png_record(s, w, (uint32_t)width, (uint32_t)height,
					FRAME_FLAG_PATCH, payload, payload_len);
			free(payload);
		}
		free(png);
		png_prev_update(w, rgba, width, height, drm_format);
		png_cursor_snapshot(w);
		return;
	}

	unsigned long png_size = 0;
	unsigned char *png = encode_png(rgba, stride, width, height, drm_format, &png_size);
	if (png == NULL) {
		return;
	}
	send_png_record(s, w, (uint32_t)width, (uint32_t)height,
			FRAME_FLAG_KEYFRAME, png, png_size);
	free(png);
	png_prev_update(w, rgba, width, height, drm_format);
	png_cursor_snapshot(w);
}

void stream_send_frame(struct mixcomp_server *s, struct mixcomp_window *w,
		const unsigned char *rgba, int stride, int width, int height,
		uint32_t drm_format) {
	if (!s->sock_ready) {
		return;
	}
	if (w->png_mode) {
		stream_send_png_frame(s, w, rgba, stride, width, height, drm_format);
		return;
	}
	uint8_t rec_type = REC_VIDEO;
	unsigned char *enc_data = NULL;
	unsigned long enc_size = 0;
	uint16_t flags = 0;
#ifdef HAVE_OPENH264
	if (w->h264 != NULL) {
		// 窗口尺寸变化 → 重建编码器
		if (w->h264_w != width || w->h264_h != height) {
			h264_close(w->h264);
			w->h264 = h264_open(width, height, w->fps);
			w->h264_w = width;
			w->h264_h = height;
			w->h264_idr_pending = w->h264 != NULL;
			if (w->h264 == NULL) {
				stream_send_info(s, (int)w->id, "jpeg");
			}
		}
	}
	if (w->h264 != NULL) {
		int key = 0;
		enc_data = h264_encode(w->h264, rgba, stride, width, height,
				w->h264_idr_pending, &enc_size, &key);
		w->h264_idr_pending = false;
		if (enc_data != NULL) {
			rec_type = REC_VIDEO_H264;
			flags = key ? FRAME_FLAG_KEYFRAME : 0;
		}
	}
#endif
	if (rec_type == REC_VIDEO) {
		// JPEG 路径(v1 语义:每帧即关键帧)
		enc_data = encode_jpeg(rgba, stride, width, height, drm_format,
				w->jpeg_quality, &enc_size);
		flags = FRAME_FLAG_KEYFRAME;
	}
	if (enc_data == NULL || enc_size == 0) {
		free(enc_data);
		return;
	}
	uint8_t buf[FRAME_HDR];
	buf[0] = 0x01; // ChanVideo
	uint32_t id = w->id, wd = (uint32_t)width, ht = (uint32_t)height;
	uint32_t sz = (uint32_t)enc_size;
	memcpy(buf + 1, &id, 4);
	memcpy(buf + 5, &wd, 4);
	memcpy(buf + 9, &ht, 4);
	memcpy(buf + 13, &sz, 4);
	if (w->have_cursor) {
		flags |= FRAME_FLAG_HASCURSOR;
	}
	memcpy(buf + 17, &flags, 2);
	int16_t cx = (int16_t)w->cursor_x, cy = (int16_t)w->cursor_y;
	uint16_t cw = 16, ch = 16;
	memcpy(buf + 19, &cx, 2);
	memcpy(buf + 21, &cy, 2);
	memcpy(buf + 23, &cw, 2);
	memcpy(buf + 25, &ch, 2);

	// 帧记录：头 + 编码数据一次写出。
	size_t total = 5 + FRAME_HDR + enc_size;
	uint8_t *rec = malloc(total);
	if (rec != NULL) {
		uint32_t len = FRAME_HDR + (uint32_t)enc_size;
		memcpy(rec, &len, 4);
		rec[4] = rec_type;
		memcpy(rec + 5, buf, FRAME_HDR);
		memcpy(rec + 5 + FRAME_HDR, enc_data, enc_size);
		rec_send_rc rc = rec_send_raw(s->sock_fd, rec, total);
		if (rc == REC_SEND_FATAL) {
			stream_mark_dead(s);
		} else if (rc == REC_SEND_AGAIN) {
			// 拥塞：丢帧，实时性优先（零字节写出，定界完好）。
		}
		free(rec);
	}
	free(enc_data);
}

// stream_recv_record —— 从累积缓冲取一条完整记录；不足则再收一轮。
// 返回 1=完整记录 0=本轮暂无更多数据 -1=连接死亡。
// 半条记录（含半条头）留在 rx_buf 跨轮累积，不再按失败处理——
// 非阻塞读被拆包是常态，误杀连接会引发无谓的断线重连风暴。
static int stream_recv_record(struct mixcomp_server *s, uint8_t *type,
		uint8_t **payload, size_t *len) {
	for (;;) {
		if (s->rx_have >= 5) {
			uint32_t blen = (uint32_t)s->rx_buf[0] |
					((uint32_t)s->rx_buf[1] << 8) |
					((uint32_t)s->rx_buf[2] << 16) |
					((uint32_t)s->rx_buf[3] << 24);
			if (blen > (16u << 20)) {
				return -1;
			}
			size_t total = 5 + (size_t)blen;
			if (s->rx_have >= total) {
				uint8_t *buf = malloc(blen ? blen : 1);
				if (buf == NULL) {
					return -1;
				}
				uint8_t t = s->rx_buf[4];
				memcpy(buf, s->rx_buf + 5, blen);
				memmove(s->rx_buf, s->rx_buf + total, s->rx_have - total);
				s->rx_have -= total;
				*type = t;
				*payload = buf;
				*len = blen;
				return 1;
			}
		}
		if (s->rx_have == s->rx_cap) {
			size_t ncap = s->rx_cap ? s->rx_cap * 2 : 4096;
			if (ncap > (32u << 20)) {
				return -1; // 单条上限 16MiB 之下还在涨，按协议违规断线
			}
			uint8_t *nb = realloc(s->rx_buf, ncap);
			if (nb == NULL) {
				return -1;
			}
			s->rx_buf = nb;
			s->rx_cap = ncap;
		}
		ssize_t n = recv(s->sock_fd, s->rx_buf + s->rx_have,
				s->rx_cap - s->rx_have, 0);
		if (n == 0) {
			return -1;
		}
		if (n < 0) {
			if (errno == EAGAIN || errno == EWOULDBLOCK) {
				return 0;
			}
			return -1;
		}
		s->rx_have += (size_t)n;
	}
}

static struct mixcomp_window *find_window(struct mixcomp_server *s, int id) {
	struct mixcomp_window *w;
	wl_list_for_each(w, &s->windows, link) {
		if ((int)w->id == id) {
			return w;
		}
	}
	return NULL;
}

// stream_poll_control —— 主循环里轮询 mixumd 的控制消息。
void stream_poll_control(struct mixcomp_server *s) {
	if (!s->sock_ready) {
		return;
	}
	for (int i = 0; i < 16; i++) { // 每轮最多处理 16 条，避免饿死捕获
		uint8_t type;
		uint8_t *payload;
		size_t len;
		int r = stream_recv_record(s, &type, &payload, &len);
		if (r <= 0) {
			if (r < 0) {
				stream_mark_dead(s);
			}
			return;
		}
		if (type == REC_CONTROL) {
			wlr_log(WLR_DEBUG, "控制消息: %.*s", (int)(len > 120 ? 120 : len),
					(const char *)payload);
			json_object *root = json_tokener_parse((const char *)payload);
			if (root != NULL) {
				json_object *jt;
				if (json_object_object_get_ex(root, "type", &jt)) {
					const char *t = json_object_get_string(jt);
					if (strcmp(t, "stream.start") == 0 ||
							strcmp(t, "stream.stop") == 0) {
						json_object *jid, *jfps, *jcodec, *jquality;
						int id = 0, fps = 30, quality = 0;
						const char *codec = "jpeg";
						bool start = strcmp(t, "stream.start") == 0;
						if (json_object_object_get_ex(root, "id", &jid)) {
							id = json_object_get_int(jid);
						}
						if (start && json_object_object_get_ex(root, "fps", &jfps)) {
							fps = json_object_get_int(jfps);
							if (fps <= 0 || fps > 60) {
								fps = 30;
							}
						}
						if (start && json_object_object_get_ex(root, "codec", &jcodec)) {
							codec = json_object_get_string(jcodec);
						}
						if (start && json_object_object_get_ex(root, "quality", &jquality)) {
							quality = json_object_get_int(jquality);
						}
						struct mixcomp_window *w = find_window(s, id);
						if (w != NULL) {
							w->want_stream = start;
							w->fps = fps;
							w->png_mode = start && strcmp(codec, "png") == 0;
							// 每次 (重新) 订阅都强制首帧全量，避免补丁落到旧画布。
							w->have_prev = false;
							if (start) {
								if (quality > 0) {
									w->jpeg_quality = quality > 100 ? 100 : quality;
								} else {
									w->jpeg_quality = s->jpeg_quality;
								}
							}
#ifdef HAVE_OPENH264
							if (!start || w->png_mode) {
								if (w->h264 != NULL) {
									h264_close(w->h264);
									w->h264 = NULL;
								}
								w->h264_idr_pending = false;
							} else if (strcmp(codec, "h264") == 0) {
								if (w->h264 == NULL && w->width > 0) {
									w->h264 = h264_open(w->width, w->height, fps);
									w->h264_w = w->width;
									w->h264_h = w->height;
								}
								w->h264_idr_pending = true;
								// 能力回告:openh264 缺失/建失败时回 jpeg
								stream_send_info(s, id,
										w->h264 != NULL ? "h264" : "jpeg");
							} else {
								if (w->h264 != NULL) {
									h264_close(w->h264);
									w->h264 = NULL;
								}
								w->h264_idr_pending = false;
							}
#endif
						}
					} else if (strcmp(t, "input") == 0 ||
							strcmp(t, "input.mode") == 0) {
						input_handle_json(s, (const char *)payload, len);
					}
				}
				json_object_put(root);
			}
		}
		free(payload);
	}
}
