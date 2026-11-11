/* Mali's Portal buffers may be ARM tiled while MPP requires linear input.
 * EGL imports the exact DRM modifier and renders into an MPP-owned DMA-BUF.
 * No glReadPixels, CPU mapping, or guessed interpretation of tiled storage. */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <xf86drm.h>
struct uu_mpp_egl {
    int fd; struct gbm_device *gbm; EGLDisplay display; EGLContext context;
    PFNEGLCREATEIMAGEKHRPROC create_image; PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_texture;
    GLuint program,source,target,fbo; EGLImageKHR target_image;
};
static void egl_close(struct uu_mpp_egl *e) {
    if (!e) return;
    if (e->display!=EGL_NO_DISPLAY) {
        if (e->context!=EGL_NO_CONTEXT && eglMakeCurrent(e->display,EGL_NO_SURFACE,EGL_NO_SURFACE,e->context)) {
            glDeleteFramebuffers(1,&e->fbo); glDeleteTextures(1,&e->source); glDeleteTextures(1,&e->target);
            if(e->program) glDeleteProgram(e->program);
            if(e->target_image!=EGL_NO_IMAGE_KHR) e->destroy_image(e->display,e->target_image);
        }
        eglMakeCurrent(e->display,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
        if(e->context!=EGL_NO_CONTEXT) eglDestroyContext(e->display,e->context);
        eglTerminate(e->display);
    }
    if(e->gbm) gbm_device_destroy(e->gbm);
    if(e->fd>=0) close(e->fd);
    free(e);
}
static GLuint egl_shader(GLenum type,const char *text) {
    GLuint s=glCreateShader(type); glShaderSource(s,1,&text,NULL);glCompileShader(s);
    GLint ok=0;glGetShaderiv(s,GL_COMPILE_STATUS,&ok);if(!ok){glDeleteShader(s);return 0;}return s;
}
static struct uu_mpp_egl *egl_open(void) {
    struct uu_mpp_egl *e=calloc(1,sizeof(*e));if(!e)return NULL;e->fd=-1;
    const char *device=getenv("UU_RKMPP_DRM_DEVICE");
    e->fd=open(device?device:"/dev/dri/renderD128",O_RDWR|O_CLOEXEC);if(e->fd<0)goto fail;
    /* Reject non-DRM devices before Mesa's GBM loader sees them. Some loader
     * versions abort during cleanup for an FD which cannot answer DRM ioctls. */
    drmVersionPtr version=drmGetVersion(e->fd);
    if(!version)goto fail;
    drmFreeVersion(version);
    if(drmGetNodeTypeFromFd(e->fd)!=DRM_NODE_RENDER)goto fail;
    e->gbm=gbm_create_device(e->fd);if(!e->gbm)goto fail;
    PFNEGLGETPLATFORMDISPLAYEXTPROC platform=(void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if(!platform)goto fail;
    e->display=platform(EGL_PLATFORM_GBM_KHR,e->gbm,NULL);
    if(e->display==EGL_NO_DISPLAY || !eglInitialize(e->display,NULL,NULL) || !eglBindAPI(EGL_OPENGL_ES_API))goto fail;
    const char *ext=eglQueryString(e->display,EGL_EXTENSIONS);
    if(!ext || !strstr(ext,"EGL_EXT_image_dma_buf_import_modifiers") || !strstr(ext,"EGL_KHR_surfaceless_context"))goto fail;
    EGLConfig cfg;EGLint count=0;
    EGLint attrs[]={EGL_RENDERABLE_TYPE,EGL_OPENGL_ES2_BIT,EGL_SURFACE_TYPE,EGL_WINDOW_BIT,EGL_NONE};
    if(!eglChooseConfig(e->display,attrs,&cfg,1,&count)||!count)goto fail;
    EGLint ctx[]={EGL_CONTEXT_CLIENT_VERSION,2,EGL_NONE};
    e->context=eglCreateContext(e->display,cfg,EGL_NO_CONTEXT,ctx);
    if(e->context==EGL_NO_CONTEXT || !eglMakeCurrent(e->display,EGL_NO_SURFACE,EGL_NO_SURFACE,e->context))goto fail;
    e->create_image=(void *)eglGetProcAddress("eglCreateImageKHR");
    e->destroy_image=(void *)eglGetProcAddress("eglDestroyImageKHR");
    e->image_texture=(void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if(!e->create_image||!e->destroy_image||!e->image_texture)goto fail;
    GLuint vs=egl_shader(GL_VERTEX_SHADER,"attribute vec2 position; varying vec2 uv; void main(){uv=(position+1.0)*0.5;gl_Position=vec4(position,0.0,1.0);}");
    GLuint fs=egl_shader(GL_FRAGMENT_SHADER,"precision highp float; varying vec2 uv; uniform sampler2D pixels; void main(){gl_FragColor=texture2D(pixels,uv);}");
    if(!vs||!fs){if(vs)glDeleteShader(vs);if(fs)glDeleteShader(fs);goto fail;}
    e->program=glCreateProgram();glAttachShader(e->program,vs);glAttachShader(e->program,fs);glBindAttribLocation(e->program,0,"position");
    glLinkProgram(e->program);glDeleteShader(vs);glDeleteShader(fs);
    GLint ok=0;glGetProgramiv(e->program,GL_LINK_STATUS,&ok);if(!ok)goto fail;
    glGenTextures(1,&e->source);glGenTextures(1,&e->target);glGenFramebuffers(1,&e->fbo);
    return e;
fail: egl_close(e);return NULL;
}
static EGLImageKHR egl_dma_image(struct uu_mpp_egl *e,int fd,uint32_t w,uint32_t h,uint32_t stride,uint32_t offset,uint32_t fourcc,uint64_t modifier) {
    EGLint attrs[]={EGL_WIDTH,w,EGL_HEIGHT,h,EGL_LINUX_DRM_FOURCC_EXT,fourcc,
        EGL_DMA_BUF_PLANE0_FD_EXT,fd,EGL_DMA_BUF_PLANE0_OFFSET_EXT,offset,EGL_DMA_BUF_PLANE0_PITCH_EXT,stride,
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,(uint32_t)modifier,EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,modifier>>32,EGL_NONE};
    return e->create_image(e->display,EGL_NO_CONTEXT,EGL_LINUX_DMA_BUF_EXT,NULL,attrs);
}
static int egl_normalize(struct uu_mpp_egl **state,const UuDmaFrame *src,int dst_fd,uint32_t w,uint32_t h,uint32_t stride) {
    if(!*state) *state=egl_open();
    struct uu_mpp_egl *e=*state;if(!e)return -30;
    if(!eglMakeCurrent(e->display,EGL_NO_SURFACE,EGL_NO_SURFACE,e->context))return -31;
    if(e->target_image==EGL_NO_IMAGE_KHR) {
        e->target_image=egl_dma_image(e,dst_fd,w,h,stride,0,0x34325241,0);
        if(e->target_image==EGL_NO_IMAGE_KHR)return -32;
        glBindTexture(GL_TEXTURE_2D,e->target);e->image_texture(GL_TEXTURE_2D,e->target_image);
        glBindFramebuffer(GL_FRAMEBUFFER,e->fbo);glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,e->target,0);
        if(glCheckFramebufferStatus(GL_FRAMEBUFFER)!=GL_FRAMEBUFFER_COMPLETE)return -33;
    }
    EGLImageKHR image=egl_dma_image(e,src->fd,src->width,src->height,src->stride,src->offset,src->fourcc,src->modifier);
    if(image==EGL_NO_IMAGE_KHR)return -34;
    glActiveTexture(GL_TEXTURE0);glBindTexture(GL_TEXTURE_2D,e->source);e->image_texture(GL_TEXTURE_2D,image);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MIN_FILTER,GL_LINEAR);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_MAG_FILTER,GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_WRAP_S,GL_CLAMP_TO_EDGE);glTexParameteri(GL_TEXTURE_2D,GL_TEXTURE_WRAP_T,GL_CLAMP_TO_EDGE);
    const GLfloat vertices[]={-1,-1,1,-1,-1,1,1,1};
    glBindFramebuffer(GL_FRAMEBUFFER,e->fbo);glViewport(0,0,w,h);glUseProgram(e->program);
    glUniform1i(glGetUniformLocation(e->program,"pixels"),0);
    glVertexAttribPointer(0,2,GL_FLOAT,GL_FALSE,0,vertices);glEnableVertexAttribArray(0);glDrawArrays(GL_TRIANGLE_STRIP,0,4);
    /* Complete all reads before releasing the producer lease, even on GL errors. */
    glFinish();GLenum error=glGetError();e->destroy_image(e->display,image);
    return error==GL_NO_ERROR?0:-35;
}
