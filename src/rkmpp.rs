//! Optional Portal DMA-BUF -> MPP encoding; RGA/EGL normalize hardware input.
use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
use std::time::{Duration,Instant};
use tokio::sync::{mpsc,Semaphore,OwnedSemaphorePermit};
use crate::capture_setting::VideoCodec;

#[repr(C)]
struct ConfigFfi { width:u32,height:u32,out_width:u32,out_height:u32,fps:u32,gop:u32,target_bps:u32,max_bps:u32,hevc:u32 }
#[repr(C)]
struct Opaque { _private:[u8;0] }
extern "C" {
    fn uu_mpp_create(config:*const ConfigFfi)->*mut Opaque;
    fn uu_mpp_destroy(engine:*mut Opaque);
    fn uu_mpp_begin(engine:*mut Opaque,pitch:*mut u32)->*mut u8;
    fn uu_mpp_encode_dma(engine:*mut Opaque,frame:*const crate::portal::DmaFrame,idr:i32,data:*mut *const u8,length:*mut u32)->i32;
    fn uu_mpp_encode(engine:*mut Opaque,idr:i32,data:*mut *const u8,length:*mut u32)->i32;
}
#[derive(Clone,Copy)]
pub struct Config { pub input:(u32,u32),pub output:(u32,u32),pub fps:u32,pub gop:u32,pub max_bps:u32,pub codec:VideoCodec }
struct Engine { pointer:*mut Opaque,height:u32 }
impl Drop for Engine { fn drop(&mut self) { unsafe { uu_mpp_destroy(self.pointer) }; } }
impl Engine {
    fn new(c:&Config)->Result<Self,String> {
        let ffi=ConfigFfi {width:c.input.0,height:c.input.1,out_width:c.output.0,out_height:c.output.1,
            fps:c.fps,gop:c.gop,target_bps:c.max_bps*3/4,max_bps:c.max_bps,hevc:u32::from(c.codec==VideoCodec::H265)};
        let pointer=unsafe {uu_mpp_create(&ffi)};
        if pointer.is_null() { return Err("Native MPP initialization failed".into()); }
        Ok(Self {pointer,height:c.input.1})
    }
    fn input(&mut self)->Result<(&mut [u8],usize),String> {
        let mut pitch=0;
        let pointer=unsafe {uu_mpp_begin(self.pointer,&mut pitch)};
        if pointer.is_null() {return Err("Native MPP input mapping failed".into());}
        // C owns the buffer until Engine::drop. No encode/device access occurs while borrowed.
        Ok((unsafe {std::slice::from_raw_parts_mut(pointer,pitch as usize*self.height as usize)},pitch as usize))
    }
    fn encode_dma(&mut self,frame:&crate::portal::DmaFrame,idr:bool)->Result<(Vec<u8>,&'static str),String> {
        let mut pointer=std::ptr::null();let mut length=0;
        // Caller holds Portal's lease through completion, including failed submissions.
        let code=unsafe {uu_mpp_encode_dma(self.pointer,frame,i32::from(idr),&mut pointer,&mut length)};
        if code<0 {return Err(format!("Native MPP DMA-BUF failed: stage={code}"));}
        if pointer.is_null() || length==0 || length>8*1024*1024 {return Err("Native MPP invalid DMA packet size".into());}
        let transform=match code {0=>"none",1=>"rga",2=>"egl",_=>return Err("Native MPP invalid DMA transform".into())};
        Ok((unsafe {std::slice::from_raw_parts(pointer,length as usize)}.to_vec(),transform))
    }
    fn encode(&mut self,idr:bool)->Result<Vec<u8>,String> {
        let mut pointer=std::ptr::null();let mut length=0;
        let code=unsafe {uu_mpp_encode(self.pointer,i32::from(idr),&mut pointer,&mut length)};
        if code!=0 {return Err(format!("Native MPP encode failed: stage={code}"));}
        if pointer.is_null() || length==0 || length>8*1024*1024 {return Err("Native MPP invalid packet size".into());}
        // The MppPacket remains owned by C until the next encode/destroy.
        Ok(unsafe {std::slice::from_raw_parts(pointer,length as usize)}.to_vec())
    }
}
pub fn selected()->bool {
    std::env::var("UU_VIDEO_PIPELINE").as_deref()!=Ok("process") && std::env::var("UU_RKMPP_RGB").as_deref()!=Ok("0")
}
pub struct Unit {pub data:Vec<u8>,pub permit:OwnedSemaphorePermit,pub completed_at:Instant}

pub async fn stream_slot(idr:&Arc<AtomicBool>)->tokio::sync::OwnedMutexGuard<()> {
    type Slots=Vec<(std::sync::Weak<AtomicBool>,Arc<tokio::sync::Mutex<()>>)>;
    static SLOTS:std::sync::Mutex<Slots>=std::sync::Mutex::new(Vec::new());
    let slot={
        let mut slots=SLOTS.lock().unwrap_or_else(|e|e.into_inner());
        slots.retain(|(key,_)|key.strong_count()>0);
        match slots.iter().find(|(key,_)|std::ptr::eq(key.as_ptr(),Arc::as_ptr(idr))) {
            Some((_,slot))=>slot.clone(),
            None=>{let slot=Arc::new(tokio::sync::Mutex::new(()));slots.push((Arc::downgrade(idr),slot.clone()));slot}
        }
    };
    slot.lock_owned().await
}
pub fn run(config:Config,portal:Arc<crate::portal::PortalSession>,units:mpsc::Sender<Unit>,
    stop:Arc<AtomicBool>,idr:Arc<AtomicBool>,budget:Arc<Semaphore>)->Result<(),String> {
    use crate::portal::DmaCapture;
    let mut version=None;let mut dma_version=None;
    let mut packed=vec![0; if config.input.0%16!=0 {config.input.0 as usize*config.input.1 as usize*4} else {0}];
    let mut dma_disabled=std::env::var("UU_PORTAL_DMABUF").as_deref()==Ok("0");
    let mut dma_active=false;
    run_with_encoder(config,move |engine,idr| {
        let capture=if dma_disabled {Ok(DmaCapture::Cpu)} else {
            portal.with_dmabuf(&mut dma_version,|frame,_|engine.encode_dma(frame,idr))
        };
        match capture {
            Ok(DmaCapture::Frame((data,transform)))=>{
                if !dma_active {
                    log::info!("Portal MPP capture=dma-buf zero_cpu_copy=true gpu_transform={transform}");
                    dma_active=true;
                }
                // A later host-copy transition must refresh the separate CPU buffer.
                version=None;
                return Ok(Some(data));
            },
            Ok(DmaCapture::Pending)=>return Ok(None),
            Ok(DmaCapture::Blank)=>{
                let (frame,_)=engine.input()?;
                for pixel in frame.chunks_exact_mut(4) {pixel.copy_from_slice(&[0,0,0,255]);}
                version=None;dma_active=false;
                return engine.encode(idr).map(Some);
            },
            Ok(DmaCapture::Cpu)=>(),
            result=>{
                match result {
                    Ok(DmaCapture::Unavailable(code))=>log::warn!("Portal MPP DMA-BUF unavailable: code={code}; capture=host-copy"),
                    Err(error)=>log::warn!("Portal MPP DMA-BUF unavailable: {error}; capture=host-copy"),
                    _=>unreachable!(),
                }
                // The C wrapper drains/destroys submitted work before releasing a failed lease.
                // Recreate an encoder which may have been reset after a hardware timeout.
                dma_disabled=true;portal.disable_dmabuf();
                *engine=Engine::new(&config)?;version=None;dma_active=false;
            },
        }
        if dma_active {version=None;dma_active=false;}
        let (frame,pitch)=engine.input()?;
        if pitch==config.input.0 as usize*4 {
            portal.capture_changed(frame,&mut version)?;
        } else {
            portal.capture_changed(&mut packed,&mut version)?;
            for (dst,src) in frame.chunks_exact_mut(pitch).zip(packed.chunks_exact(config.input.0 as usize*4)) {
                dst[..src.len()].copy_from_slice(src);
            }
        }
        engine.encode(idr).map(Some)
    },units,stop,idr,budget)
}
fn run_with_encoder(config:Config,mut encode:impl FnMut(&mut Engine,bool)->Result<Option<Vec<u8>>,String>,
    units:mpsc::Sender<Unit>,stop:Arc<AtomicBool>,idr:Arc<AtomicBool>,budget:Arc<Semaphore>)->Result<(),String> {
    let mut engine=Engine::new(&config)?;
    let interval=Duration::from_nanos(1_000_000_000/u64::from(config.fps));
    let mut next=Instant::now();
    while !stop.load(Ordering::Relaxed) && !units.is_closed() {
        if let Some(delay)=next.checked_duration_since(Instant::now()) {std::thread::sleep(delay);}
        let permit=tokio::runtime::Handle::current().block_on(async {
            tokio::select! { permit=budget.clone().acquire_owned()=>permit.ok(),_=units.closed()=>None }
        });
        let Some(permit)=permit else {break};
        if stop.load(Ordering::Relaxed) || units.is_closed() {break;}
        let force_idr=idr.swap(false,Ordering::Relaxed);
        let data=encode(&mut engine,force_idr)?;
        next=(next+interval).max(Instant::now());
        let Some(data)=data else {
            if force_idr {idr.store(true,Ordering::Relaxed);}
            continue;
        };
        if units.blocking_send(Unit {data,permit,completed_at:Instant::now()}).is_err() {break;}
    }
    Ok(())
}
pub fn black_frame(codec:VideoCodec)->Result<bytes::Bytes,String> {
    let config=Config {input:(1280,720),output:(1280,720),fps:1,gop:1,max_bps:2_000_000,codec};
    let mut engine=Engine::new(&config)?;
    let (pixels,_)=engine.input()?;
    for pixel in pixels.chunks_exact_mut(4) {pixel.copy_from_slice(&[0,0,0,255]);}
    Ok(engine.encode(true)?.into())
}
