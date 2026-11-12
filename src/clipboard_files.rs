//! Windows file clipboard bridge. Copy synchronizes metadata; reads transfer content.
//! Wire evidence: docs/clipboard-files-protocol.md. Paths and contents are never logged.
use crate::clipboard_rich::{bytes,header,int,number,request,response,text_field};
use crate::protocol::{decode_wire,WireValue};
use crate::clipboard_fs;
use std::{collections::{BTreeMap,HashMap,HashSet},sync::{Arc,mpsc},time::{Duration,Instant}};
use tokio::sync::mpsc::{UnboundedSender,UnboundedReceiver};
const LIMIT:u64=1024*1024*1024;
const MAX_ENTRIES:usize=10_000;
const MAX_METADATA:usize=16*1024*1024;
pub struct Read {transfer:u64,index:u32,offset:u64,size:u32,reply:mpsc::SyncSender<Result<Vec<u8>,()>>}
impl Read {pub fn reject(self){let _=self.reply.send(Err(()));}}
struct Pending {index:u32,offset:u64,size:u32,reply:mpsc::SyncSender<Result<Vec<u8>,()>>,sent:Instant}
struct Info {name:String,directory:bool,size:u64,modified:u64}
pub struct State {
    transfer:Option<u64>,selection:Option<Arc<clipboard_fs::Selection>>,files:HashMap<u32,Arc<clipboard_fs::File>>,
    reads:UnboundedSender<Read>,pending:HashMap<u64,Pending>,count:Option<usize>,blocks:BTreeMap<usize,Vec<Vec<u8>>>,metadata_bytes:usize,local:Option<Local>,
}
#[derive(Default)]
pub struct Actions {pub messages:Vec<(u32,Vec<u8>)>,pub offer:Option<Arc<clipboard_fs::Selection>>,pub clear:bool}
fn info(data:&[u8])->Result<Info,&'static str> {
    let attributes=number(data,2)?;let directory=attributes&0x10!=0;
    let name=text_field(data,1)?.replace('\\',"/");
    let name=if directory{name.trim_end_matches('/')}else{&name};
    if name.is_empty() || name.starts_with('/') || name.len()>4096 || name.chars().any(|c|c.is_control() || c==':')
        || name.split('/').any(|part|part.is_empty() || part.len()>255 || part=="." || part=="..") {
        return Err("unsafe clipboard relative path");
    }
    if attributes&0x400!=0{return Err("clipboard reparse points are not supported");}
    let size=if directory{0}else{number(data,4)?};
    if size>LIMIT{return Err("clipboard selection exceeds 1 GiB limit");}
    Ok(Info{name:name.to_owned(),directory,size,modified:number(data,3)?})
}
impl State {
    pub fn new()->(Self,UnboundedReceiver<Read>){
        let(reads,rx)=tokio::sync::mpsc::unbounded_channel();
        (Self{transfer:None,selection:None,files:HashMap::new(),reads,pending:HashMap::new(),count:None,blocks:BTreeMap::new(),metadata_bytes:0,local:None},rx)
    }
    fn finish_list(&mut self)->Result<Option<Arc<clipboard_fs::Selection>>,&'static str>{
        if self.selection.is_some(){return Ok(None);}
        let Some(count)=self.count else{return Ok(None);};
        let first=if (0..count).all(|n|self.blocks.contains_key(&n)){0}else if (1..=count).all(|n|self.blocks.contains_key(&n)){1}else{return Ok(None);};
        let mut entries=Vec::new();let mut total=0u64;
        for n in first..first+count{for entry in self.blocks[&n].iter().map(|data|info(data)).collect::<Result<Vec<_>,_>>()?{total=total.checked_add(entry.size).ok_or("clipboard size overflow")?;entries.push(entry);if entries.len()>MAX_ENTRIES || total>LIMIT{return Err("clipboard selection exceeds limits");}}}
        let mut paths=HashMap::new();
        for entry in &entries{if paths.insert(entry.name.clone(),entry.directory).is_some(){return Err("duplicate clipboard relative path");}}
        let file_paths:HashSet<&str>=paths.iter().filter_map(|(path,dir)|(!dir).then_some(path.as_str())).collect();
        for entry in &entries{let mut parent=entry.name.as_str();while let Some((prefix,_))=parent.rsplit_once('/'){if file_paths.contains(prefix){return Err("clipboard file collides with directory");}parent=prefix;}}
        let transfer=self.transfer.ok_or("clipboard transfer missing")?;
        let mut files=Vec::new();let mut directories=Vec::new();
        for(index,entry)in entries.into_iter().enumerate(){
            if entry.directory{directories.push(clipboard_fs::Directory{name:entry.name,modified:entry.modified});continue;}
            let reads=self.reads.clone();let index=index as u32;
            let fetch:clipboard_fs::Fetch=Arc::new(move|offset,size|{
                let(tx,rx)=mpsc::sync_channel(1);reads.send(Read{transfer,index,offset,size,reply:tx}).map_err(|_|())?;
                rx.recv_timeout(Duration::from_secs(20)).map_err(|_|())?
            });
            let file=clipboard_fs::File::new(entry.name,entry.size,entry.modified,fetch);self.files.insert(index,file.clone());files.push(file);
        }
        log::info!("Clipboard virtual selection ready: files={} directories={} bytes={} cached_bytes=0",files.len(),directories.len(),total);
        let selection=clipboard_fs::Selection::new(files,directories);self.selection=Some(selection.clone());Ok(Some(selection))
    }
    pub fn handle(&mut self,kind:u32,rpc:&[u8],next:&mut impl FnMut()->u64)->Result<Option<Actions>,&'static str>{
        let Some(clip)=bytes(rpc,if kind==21{9}else{5})?else{return Ok(None);};
        if kind==21 {
            if let Some(result)=self.serve(rpc,clip,next)?{return Ok(Some(result));}
        }
        if kind==21{if let Some(list)=bytes(clip,1)?{
            self.cancel();self.local=None;let fields=decode_wire(list).map_err(|_|"invalid clipboard formats")?;
            let has=|name|fields.iter().filter(|f|f.number==1).any(|f|match f.value{WireValue::Bytes(data)=>[2,3].iter().any(|&n|text_field(data,n)==Ok(name)),_=>false});
            if !has("FileGroupDescriptorW") || !has("FileContents"){return Ok(None);}
            let transfer=u64::from((rand::random::<u32>()&0x7fffffff).max(1));self.transfer=Some(transfer);
            let mut ask=Vec::new();int(1,transfer,&mut ask);log::info!("Clipboard file list requested");
            return Ok(Some(Actions{messages:vec![(22,response(header(rpc)?,1,&[])),(21,request(next(),4,&ask))],offer:None,clear:true}));
        }}
        let Some(transfer)=self.transfer else{return Ok(None);};
        if rpc.len()>MAX_METADATA+2048{return Err("clipboard message exceeds range limit");}
        for field in decode_wire(clip).map_err(|_|"invalid clipboard file RPC")?{
            let WireValue::Bytes(body)=field.value else{continue;};if number(body,1).ok()!=Some(transfer){continue;}
            if kind==22 && field.number==4{
                let count=usize::try_from(number(body,2)?).map_err(|_|"invalid clipboard list count")?;
                if number(body,3)?!=1 || count==0 || count>MAX_ENTRIES{return Err("clipboard provider did not offer a supported selection");}
                if self.count.is_some_and(|old|old!=count){return Err("conflicting clipboard list count");}self.count=Some(count);
                return Ok(Some(Actions{offer:self.finish_list()?,..Actions::default()}));
            }
            if kind==22 && field.number==5{
                let id=number(header(rpc)?,1)?;
                if let Some(pending)=self.pending.remove(&id){
                    let data=bytes(body,2)?.unwrap_or(&[]);
                    if number(body,3)?==1 && data.len()==pending.size as usize{let _=pending.reply.send(Ok(data.to_vec()));}
                    else{let _=pending.reply.send(Err(()));log::warn!("Clipboard file range rejected: index={} offset={} requested={} received={} status={}",pending.index,pending.offset,pending.size,data.len(),number(body,3)?);}
                }
                return Ok(Some(Actions::default()));
            }
            if kind==21 && field.number==7{
                // Official clipboard.proto: desc_segment.file_descs is repeated field 3.
                // Taking only bytes(body,3) silently discarded every entry after the folder.
                let data=decode_wire(body).map_err(|_|"invalid clipboard metadata segment")?.into_iter()
                    .filter_map(|f|match(f.number,f.value){(3,WireValue::Bytes(v))=>Some(v.to_vec()),_=>None}).collect::<Vec<_>>();
                if data.is_empty() || data.len()>MAX_ENTRIES{return Err("invalid clipboard metadata entry count");}
                let length=data.iter().map(Vec::len).sum::<usize>();
                let index=usize::try_from(number(body,2)?).map_err(|_|"invalid clipboard metadata index")?;
                if index>MAX_ENTRIES{return Err("clipboard metadata index exceeds limit");}
                if let Some(old)=self.blocks.get(&index){if old!=&data{return Err("conflicting clipboard metadata block");}}
                else{if self.metadata_bytes+length>MAX_METADATA{return Err("clipboard metadata exceeds limit");}self.metadata_bytes+=length;self.blocks.insert(index,data);}
                let mut ack=Vec::new();int(1,transfer,&mut ack);int(2,index as u64,&mut ack);int(3,1,&mut ack);
                return Ok(Some(Actions{messages:vec![(22,response(header(rpc)?,7,&ack))],offer:self.finish_list()?,clear:false}));
            }
        }Ok(None)
    }
    pub fn read(&mut self,read:Read,id:u64)->Option<Vec<u8>>{
        if self.transfer!=Some(read.transfer) || self.files.get(&read.index).is_none_or(|f|!f.active() || read.size>clipboard_fs::CHUNK || read.offset.checked_add(u64::from(read.size)).is_none_or(|end|end>f.size)){
            read.reject();return None;
        }
        // Windows case 5: ID, file index, range flag, position, requested bytes.
        let mut ask=Vec::new();int(1,read.transfer,&mut ask);if read.index!=0{int(2,u64::from(read.index),&mut ask);}int(3,2,&mut ask);
        if read.offset!=0{int(4,read.offset,&mut ask);}int(5,u64::from(read.size),&mut ask);
        log::debug!("Clipboard file range requested: index={} offset={} bytes={}",read.index,read.offset,read.size);
        self.pending.insert(id,Pending{index:read.index,offset:read.offset,size:read.size,reply:read.reply,sent:Instant::now()});Some(request(id,5,&ask))
    }
    pub fn expire(&mut self){self.pending.retain(|_,p|{if p.sent.elapsed()>Duration::from_secs(20){let _=p.reply.send(Err(()));log::warn!("Clipboard file range timed out: index={} offset={} bytes={}",p.index,p.offset,p.size);false}else{true}});}
    pub fn cancel(&mut self){self.local=None;self.transfer=None;if let Some(selection)=self.selection.take(){selection.invalidate();}self.files.clear();self.count=None;self.blocks.clear();self.metadata_bytes=0;for(_,p)in self.pending.drain(){let _=p.reply.send(Err(()));}}
}
struct Source {info:Info,path:std::path::PathBuf,identity:std::fs::Metadata}
struct Local {entries:Vec<Source>,segments:Vec<Vec<u8>>,task:Option<u64>}
impl Local {
    fn collect(uris:&str)->Result<Self,&'static str> {
        use std::{path::Path,os::unix::fs::MetadataExt};
        fn walk(path:&Path,name:String,entries:&mut Vec<Source>,total:&mut u64,depth:usize)->Result<(),&'static str> {
            if entries.len()>=MAX_ENTRIES || depth>64{return Err("clipboard selection exceeds entry/depth limits");}
            if name.contains('\\'){return Err("clipboard filename contains a Windows path separator");}
            let meta=std::fs::symlink_metadata(path).map_err(|_|"clipboard source metadata unavailable")?;
            if !meta.is_file() && !meta.is_dir(){return Err("clipboard links and special files are not supported");}
            let mut descriptor=Vec::new();crate::clipboard_protocol::nested(1,name.as_bytes(),&mut descriptor);
            int(2,if meta.is_dir(){0x10}else{0x20},&mut descriptor);int(3,meta.mtime().max(0) as u64,&mut descriptor);int(4,if meta.is_dir(){0}else{meta.len()},&mut descriptor);
            let entry=info(&descriptor)?;
            *total=total.checked_add(entry.size).ok_or("clipboard size overflow")?;
            if *total>LIMIT{return Err("clipboard selection exceeds 1 GiB limit");}
            let directory=entry.directory;
            entries.push(Source{info:entry,path:path.to_owned(),identity:meta});
            if directory {
                for child in std::fs::read_dir(path).map_err(|_|"clipboard directory unavailable")? {
                    let child=child.map_err(|_|"clipboard directory entry unavailable")?;
                    let child_name=child.file_name().into_string().map_err(|_|"clipboard filename is not UTF-8")?;
                    walk(&child.path(),format!("{name}/{child_name}"),entries,total,depth+1)?;
                }
            }
            Ok(())
        }
        let mut entries=Vec::new();let mut total=0;let mut roots=HashSet::new();
        for line in uris.lines().filter(|s|!s.is_empty() && !s.starts_with('#') && *s!="copy" && *s!="cut") {
            let url=reqwest::Url::parse(line).map_err(|_|"invalid clipboard file URI")?;
            let path=url.to_file_path().map_err(|_|"clipboard source must be a local file URI")?;
            // Reject a symlink root before canonicalization; do not traverse symlinks below it.
            if std::fs::symlink_metadata(&path).map_err(|_|"clipboard source unavailable")?.file_type().is_symlink(){return Err("clipboard links are not supported");}
            let name=path.file_name().and_then(|n|n.to_str()).ok_or("invalid clipboard source name")?.to_owned();
            if !roots.insert(name.clone()){return Err("duplicate clipboard root name");}
            let path=path.canonicalize().map_err(|_|"clipboard source unavailable")?;
            walk(&path,name,&mut entries,&mut total,0)?;
        }
        if entries.is_empty(){return Err("empty clipboard file selection");}
        let mut segments=Vec::new();let mut segment=Vec::new();let mut metadata=0;
        for source in &entries {
            let mut desc=Vec::new();crate::clipboard_protocol::nested(1,source.info.name.replace('/',"\\").as_bytes(),&mut desc);
            int(2,if source.info.directory{0x10}else{0x20},&mut desc);int(3,source.info.modified,&mut desc);int(4,source.info.size,&mut desc);
            let mut field=Vec::new();crate::clipboard_protocol::nested(3,&desc,&mut field);
            metadata+=field.len();if metadata>MAX_METADATA{return Err("clipboard metadata exceeds limit");}
            if segment.len()+field.len()>clipboard_fs::CHUNK as usize{segments.push(std::mem::take(&mut segment));}
            segment.extend(field);
        }
        if !segment.is_empty(){segments.push(segment);}
        log::info!("Local clipboard file selection ready: entries={} bytes={}",entries.len(),total);
        Ok(Self{entries,segments,task:None})
    }
    fn read(&self,index:usize,offset:u64,size:u32,flags:u64)->Result<Vec<u8>,()> {
        use std::os::unix::fs::{MetadataExt,OpenOptionsExt,FileExt};
        let source=self.entries.get(index).ok_or(())?;
        if source.info.directory{return Err(());}
        if source.path.canonicalize().map_err(|_|())?!=source.path{return Err(());}
        let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(&source.path).map_err(|_|())?;
        let meta=file.metadata().map_err(|_|())?;let old=&source.identity;
        if !meta.is_file() || meta.dev()!=old.dev() || meta.ino()!=old.ino() || meta.len()!=old.len() || meta.mtime()!=old.mtime() || meta.mtime_nsec()!=old.mtime_nsec(){return Err(());}
        if flags==1{return Ok(source.info.size.to_le_bytes().to_vec());}
        if flags!=2 || size>16*1024*1024 || offset>source.info.size{return Err(());}
        let mut data=vec![0;u64::from(size.min(clipboard_fs::CHUNK)).min(source.info.size-offset) as usize];
        file.read_exact_at(&mut data,offset).map_err(|_|())?;Ok(data)
    }
}
impl State {
    pub fn clear_local(&mut self){self.local=None;}
    pub fn offer_local(&mut self,id:u64,uris:&str)->Result<Vec<u8>,&'static str> {
        self.cancel();let local=Local::collect(uris)?;
        let mut list=Vec::new();
        for(id,name)in [(0xc001,"FileGroupDescriptorW"),(0xc002,"FileContents")] {
            let mut format=Vec::new();int(1,id,&mut format);crate::clipboard_protocol::nested(2,name.as_bytes(),&mut format);crate::clipboard_protocol::nested(1,&format,&mut list);
        }
        self.local=Some(local);Ok(request(id,1,&list))
    }
    fn serve(&mut self,rpc:&[u8],clip:&[u8],next:&mut impl FnMut()->u64)->Result<Option<Actions>,&'static str> {
        if let Some(ask)=bytes(clip,4)? {
            let task=number(ask,1)?;let mut reply=Vec::new();int(1,task,&mut reply);
            let mut out=Actions::default();
            if let Some(local)=&mut self.local {
                local.task=Some(task);int(2,local.segments.len() as u64,&mut reply);int(3,1,&mut reply);
                out.messages.push((22,response(header(rpc)?,4,&reply)));
                for(index,data)in local.segments.iter().enumerate() {
                    let mut segment=Vec::new();int(1,task,&mut segment);int(2,(index+1)as u64,&mut segment);segment.extend(data);
                    out.messages.push((21,request(next(),7,&segment)));
                }
            }else{int(3,2,&mut reply);out.messages.push((22,response(header(rpc)?,4,&reply)));}
            return Ok(Some(out));
        }
        if let Some(ask)=bytes(clip,5)? {
            let task=number(ask,1)?;let index=number(ask,2)?;let offset=number(ask,4)?;let mut reply=Vec::new();int(1,task,&mut reply);
            let data=self.local.as_ref().filter(|l|l.task==Some(task)).and_then(|l|{
                l.read(usize::try_from(index).ok()?,offset,u32::try_from(number(ask,5).ok()?).ok()?,number(ask,3).ok()?).ok()
            });
            if let Some(data)=&data{crate::clipboard_protocol::nested(2,data,&mut reply);}
            int(3,if data.is_some(){1}else{2},&mut reply);int(4,offset,&mut reply);int(5,index,&mut reply);
            return Ok(Some(Actions{messages:vec![(22,response(header(rpc)?,5,&reply))],..Actions::default()}));
        }
        if let Some(ask)=bytes(clip,6)? {
            let task=number(ask,1)?;
            if self.transfer==Some(task){self.cancel();}
            if self.local.as_ref().is_some_and(|l|l.task==Some(task)){self.local.as_mut().unwrap().task=None;}
            let mut reply=Vec::new();int(1,task,&mut reply);int(2,1,&mut reply);
            return Ok(Some(Actions{messages:vec![(22,response(header(rpc)?,6,&reply))],..Actions::default()}));
        }
        Ok(None)
    }
}

impl Drop for State{fn drop(&mut self){self.cancel();}}
pub fn publish(selection:Arc<clipboard_fs::Selection>,portal:&crate::portal::PortalSession)->Result<(),&'static str>{
    let paths=clipboard_fs::publish(selection)?;
    let uris=paths.into_iter().map(|path|reqwest::Url::from_file_path(path).map(|u|u.to_string()).map_err(|_|"clipboard URI conversion failed")).collect::<Result<Vec<_>,_>>()?;
    portal.clipboard_set_files(&uris.join("\n")).map_err(|_|"clipboard file offer failed")
}
