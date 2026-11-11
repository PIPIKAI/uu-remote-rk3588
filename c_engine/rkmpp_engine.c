/* Native RK3588 encoder. BGRA mapping and VBR settings follow ffmpeg-rockchip's
 * libavcodec/rkmppenc.c; buffer ownership follows the MPP mpi_enc_test example.
 * No FFmpeg APIs are used. One submitted frame is completed before buffer reuse. */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <time.h>
#include <unistd.h>
#include "dmabuf_frame.h"
#include <rockchip/rk_mpi.h>
#include <rockchip/mpp_buffer.h>
#include <rockchip/mpp_frame.h>
#include <rockchip/mpp_packet.h>
#include <rockchip/rk_venc_cfg.h>
#include <rga/im2d.h>

#include "rkmpp_egl.inc"

struct uu_mpp_config { uint32_t width,height,out_width,out_height,fps,gop,target_bps,max_bps,hevc; };
struct uu_mpp {
    MppCtx ctx; MppApi *api; MppEncCfg cfg;
    struct uu_mpp_egl *egl;
    MppBufferGroup group; MppBuffer input,output; MppPacket packet;
    struct uu_mpp_config config;
    uint32_t pitch,out_pitch,rows,out_rows; uint64_t sequence;
    int writing,import_fd; MppBuffer imported;
    uint32_t encode_pitch,encode_rows; MppFrameFormat encode_format;
};
void uu_mpp_destroy(struct uu_mpp *p) {
    if (!p) return;
    if (p->ctx && p->api) p->api->reset(p->ctx);
    if (p->packet) mpp_packet_deinit(&p->packet);
    if (p->ctx) mpp_destroy(p->ctx);
    if (p->writing) mpp_buffer_sync_end(p->input);
    egl_close(p->egl);
    if (p->imported) mpp_buffer_put(p->imported);
    if (p->import_fd>=0) close(p->import_fd);
    if (p->output) mpp_buffer_put(p->output);
    if (p->input) mpp_buffer_put(p->input);
    if (p->group) mpp_buffer_group_put(p->group);
    if (p->cfg) mpp_enc_cfg_deinit(p->cfg);
    free(p);
}
struct uu_mpp *uu_mpp_create(const struct uu_mpp_config *c) {
    if (!c || !c->width || !c->height || c->width>8192 || c->height>8192 ||
        !c->out_width || !c->out_height || c->out_width>8192 || c->out_height>8192 ||
        (c->out_width&1) || (c->out_height&1) || !c->fps || c->fps>240 || !c->gop ||
        !c->target_bps || c->target_bps>c->max_bps || c->max_bps>1000000000) return NULL;
    struct uu_mpp *p=calloc(1,sizeof(*p)); if (!p) return NULL;
    p->import_fd=-1;
    p->config=*c; p->pitch=((c->width+15)&~15)*4; p->rows=(c->height+15)&~15;
    p->out_pitch=((c->out_width+15)&~15)*4; p->out_rows=(c->out_height+15)&~15;
    if (mpp_create(&p->ctx,&p->api)) goto fail;
    RK_S64 timeout=1000;
    if (p->api->control(p->ctx,MPP_SET_INPUT_TIMEOUT,&timeout) ||
        p->api->control(p->ctx,MPP_SET_OUTPUT_TIMEOUT,&timeout) ||
        mpp_init(p->ctx,MPP_CTX_ENC,c->hevc?MPP_VIDEO_CodingHEVC:MPP_VIDEO_CodingAVC) ||
        mpp_enc_cfg_init(&p->cfg) || p->api->control(p->ctx,MPP_ENC_GET_CFG,p->cfg)) goto fail;
#define SET(key,value) do { if (mpp_enc_cfg_set_s32(p->cfg,key,value)) goto fail; } while (0)
    SET("prep:width",c->out_width); SET("prep:height",c->out_height);
    SET("prep:hor_stride",p->out_pitch); SET("prep:ver_stride",p->out_rows);
    SET("prep:format",MPP_FMT_BGRA8888);
    SET("rc:mode",MPP_ENC_RC_MODE_VBR); SET("rc:bps_target",c->target_bps);
    SET("rc:bps_max",c->max_bps); SET("rc:bps_min",c->target_bps/16);
    SET("rc:fps_in_flex",0); SET("rc:fps_in_num",c->fps); SET("rc:fps_in_denom",1);
    SET("rc:fps_out_flex",0); SET("rc:fps_out_num",c->fps); SET("rc:fps_out_denom",1);
    SET("rc:gop",c->gop); SET("rc:drop_mode",MPP_ENC_RC_DROP_FRM_DISABLED);
    SET("rc:qp_init",26); SET("rc:qp_min",0); SET("rc:qp_max",48);
    SET("rc:qp_min_i",0); SET("rc:qp_max_i",48); SET("rc:qp_ip",2);
    SET("split:mode",0);
    if (c->hevc) { SET("h265:profile",1); SET("h265:level",153); SET("h265:tier",0); }
    else { SET("h264:profile",66); SET("h264:level",52); SET("h264:cabac_en",0); SET("h264:trans8x8",0); }
#undef SET
    if (p->api->control(p->ctx,MPP_ENC_SET_CFG,p->cfg)) goto fail;
    MppEncHeaderMode header=MPP_ENC_HEADER_MODE_EACH_IDR;
    MppEncSeiMode sei=MPP_ENC_SEI_MODE_DISABLE;
    if (p->api->control(p->ctx,MPP_ENC_SET_HEADER_MODE,&header) ||
        p->api->control(p->ctx,MPP_ENC_SET_SEI_CFG,&sei) ||
        mpp_buffer_group_get_internal(&p->group,MPP_BUFFER_TYPE_DRM) ||
        mpp_buffer_get(p->group,&p->input,(size_t)p->pitch*p->rows)) goto fail;
    if (c->width!=c->out_width || c->height!=c->out_height) {
        if (mpp_buffer_get(p->group,&p->output,(size_t)p->out_pitch*p->out_rows)) goto fail;
    }
    if (mpp_buffer_sync_begin(p->input)) goto fail;
    memset(mpp_buffer_get_ptr(p->input),0,(size_t)p->pitch*p->rows);
    if (mpp_buffer_sync_end(p->input)) goto fail;
    p->encode_pitch=p->out_pitch; p->encode_rows=p->out_rows; p->encode_format=MPP_FMT_BGRA8888;
    return p;
fail: uu_mpp_destroy(p); return NULL;
}
uint8_t *uu_mpp_begin(struct uu_mpp *p,uint32_t *pitch) {
    if (!p || !pitch || p->writing || mpp_buffer_sync_begin(p->input)) return NULL;
    p->writing=1; *pitch=p->pitch; return mpp_buffer_get_ptr(p->input);
}
/* Previous imports stay alive until the packet is retired, even after the
 * PipeWire lease is released. EXT_DMA does not own/close our duplicated FD. */
static void retire_input(struct uu_mpp *p) {
    if (p->packet) mpp_packet_deinit(&p->packet);
    if (p->imported) { mpp_buffer_put(p->imported); p->imported=NULL; }
    if (p->import_fd>=0) { close(p->import_fd); p->import_fd=-1; }
}
static int abort_submission(struct uu_mpp *p,int error) {
    /* Returning an error must not release the producer lease while hardware may
     * still read its pixels. Reset/destroy joins the MPP work before returning. */
    if (p->ctx) {
        p->api->reset(p->ctx);
        if (p->packet) mpp_packet_deinit(&p->packet);
        mpp_destroy(p->ctx); p->ctx=NULL;
    }
    return error;
}
static int encode_buffer(struct uu_mpp *p,MppBuffer buffer,uint32_t pitch,uint32_t rows,
    MppFrameFormat format,int idr,const uint8_t **data,uint32_t *length) {
    if (!p->ctx) return -10;
    if (pitch!=p->encode_pitch || rows!=p->encode_rows || format!=p->encode_format) {
        if (mpp_enc_cfg_set_s32(p->cfg,"prep:hor_stride",pitch) ||
            mpp_enc_cfg_set_s32(p->cfg,"prep:ver_stride",rows) ||
            mpp_enc_cfg_set_s32(p->cfg,"prep:format",format) ||
            p->api->control(p->ctx,MPP_ENC_SET_CFG,p->cfg)) return -11;
        p->encode_pitch=pitch; p->encode_rows=rows; p->encode_format=format; idr=1;
    }
    if (idr || p->sequence%p->config.gop==0) {
        if (p->api->control(p->ctx,MPP_ENC_SET_IDR_FRAME,NULL)) return -4;
    }
    MppFrame frame=NULL;
    if (mpp_frame_init(&frame)) return -5;
    mpp_frame_set_width(frame,p->config.out_width); mpp_frame_set_height(frame,p->config.out_height);
    mpp_frame_set_hor_stride(frame,pitch); mpp_frame_set_ver_stride(frame,rows);
    mpp_frame_set_fmt(frame,format); mpp_frame_set_buffer(frame,buffer);
    mpp_frame_set_pts(frame,(RK_S64)(p->sequence*1000000/p->config.fps));
    int ret=p->api->encode_put_frame(p->ctx,frame);
    mpp_frame_deinit(&frame);
    if (ret) return abort_submission(p,-6);
    if (p->api->encode_get_packet(p->ctx,&p->packet) || !p->packet) return abort_submission(p,-7);
    if (mpp_packet_is_partition(p->packet) && !mpp_packet_is_eoi(p->packet)) return abort_submission(p,-8);
    size_t len=mpp_packet_get_length(p->packet);
    if (!len || len>8*1024*1024) return abort_submission(p,-9);
    *data=mpp_packet_get_pos(p->packet); *length=(uint32_t)len; p->sequence++;
    return 0;
}
int uu_mpp_encode(struct uu_mpp *p,int idr,const uint8_t **data,uint32_t *length) {
    if (!p || !p->ctx || !p->writing || !data || !length) return -1;
    *data=NULL; *length=0;
    if (mpp_buffer_sync_end(p->input)) return -2;
    p->writing=0; retire_input(p);
    MppBuffer buffer=p->input;
    if (p->output) {
        struct uu_mpp_config *c=&p->config;
        rga_buffer_t src=wrapbuffer_fd_t(mpp_buffer_get_fd(p->input),c->width,c->height,p->pitch/4,p->rows,RK_FORMAT_BGRA_8888);
        rga_buffer_t dst=wrapbuffer_fd_t(mpp_buffer_get_fd(p->output),c->out_width,c->out_height,p->out_pitch/4,p->out_rows,RK_FORMAT_BGRA_8888);
        if (imresize_t(src,dst,0,0,INTER_LINEAR,1)!=IM_STATUS_SUCCESS) return -3;
        buffer=p->output;
    }
    return encode_buffer(p,buffer,p->out_pitch,p->out_rows,MPP_FMT_BGRA8888,idr,data,length);
}
static int64_t monotonic_ms(void) {
    struct timespec t; if (clock_gettime(CLOCK_MONOTONIC,&t)) return -1;
    return (int64_t)t.tv_sec*1000+t.tv_nsec/1000000;
}
static int wait_producer(int fd) {
    int64_t start=monotonic_ms(); if (start<0) return -1;
    for (;;) {
        int64_t now=monotonic_ms(); if (now<0 || now-start>=1000) return -1;
        struct pollfd pollfd={.fd=fd,.events=POLLIN};
        int ret=poll(&pollfd,1,(int)(1000-(now-start)));
        if (ret<0 && errno==EINTR) continue;
        return ret>0 && (pollfd.revents&POLLIN) && !(pollfd.revents&(POLLERR|POLLHUP|POLLNVAL))?0:-1;
    }
}
/* Called only inside Portal's lease: linear RGB or the exact ARM tiled modifier.
 * EGL imports tiled storage; MPP/RGA must never interpret it as linear pixels.
 * Success: 0=direct import, 1=RGA, 2=EGL normalization; errors are negative. */
int uu_mpp_encode_dma(struct uu_mpp *p,const UuDmaFrame *f,int idr,const uint8_t **data,uint32_t *length) {
    if (!p || !p->ctx || p->writing || !f || !data || !length) return -20;
    *data=NULL; *length=0;
    if (f->fd<0 || (f->modifier && f->modifier!=UINT64_C(0x0810000000000001)) || f->offset || f->width!=p->config.width || f->height!=p->config.height ||
        f->stride<f->width*4 || f->stride%4 || (uint64_t)f->stride*f->height>f->size ||
        f->stride/4>8192 || (f->modifier && (f->stride%64 ||
        (uint64_t)f->stride*((f->height+15)&~15u)>f->size))) return -21;
    MppFrameFormat format; int rga_format;
    switch (f->fourcc) {
        case 0x34325241: case 0x34325258: format=MPP_FMT_BGRA8888; rga_format=RK_FORMAT_BGRA_8888; break;
        case 0x34324241: case 0x34324258: format=MPP_FMT_RGBA8888; rga_format=RK_FORMAT_RGBA_8888; break;
        default:return -22;
    }
    if (wait_producer(f->fd)) return -23;
    retire_input(p);
    int fd=fcntl(f->fd,F_DUPFD_CLOEXEC,0); if (fd<0) return -24;
    MppBufferInfo info={.type=MPP_BUFFER_TYPE_EXT_DMA,.size=f->size,.fd=fd};
    if (mpp_buffer_import(&p->imported,&info)) { close(fd); return -25; }
    p->import_fd=fd;
    uint32_t rows=(f->height+15)&~15;
    int transform=f->modifier || p->config.out_width!=f->width || p->config.out_height!=f->height ||
        f->stride%16 || (uint64_t)f->stride*rows>f->size;
    MppBuffer buffer=p->imported;
    uint32_t pitch=f->stride;
    if (transform) {
        if (!p->output && mpp_buffer_get(p->group,&p->output,(size_t)p->out_pitch*p->out_rows)) return -26;
        rga_buffer_t src=wrapbuffer_fd_t(fd,f->width,f->height,f->stride/4,f->height,rga_format);
        rga_buffer_t dst=wrapbuffer_fd_t(mpp_buffer_get_fd(p->output),p->config.out_width,p->config.out_height,
            p->out_pitch/4,p->out_rows,RK_FORMAT_BGRA_8888);
        if (f->modifier) {
            int ret=egl_normalize(&p->egl,f,mpp_buffer_get_fd(p->output),p->config.out_width,p->config.out_height,p->out_pitch);
            if (ret) return ret;
        } else if (imresize_t(src,dst,0,0,INTER_LINEAR,1)!=IM_STATUS_SUCCESS) return -27;
        buffer=p->output; pitch=p->out_pitch; rows=p->out_rows; format=MPP_FMT_BGRA8888;
    }
    int ret=encode_buffer(p,buffer,pitch,rows,format,idr,data,length);
    return ret?ret:f->modifier?2:transform;
}
