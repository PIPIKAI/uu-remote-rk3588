//! Read-only clipboard files. Metadata never creates a data cache or fetches bytes.
use std::{collections::BTreeSet,fs::File as DiskFile,io,os::unix::{fs::{DirBuilderExt,OpenOptionsExt,FileExt}},path::{Path,PathBuf},sync::{Arc,Mutex,atomic::{AtomicBool,AtomicUsize,Ordering}}};
pub const CHUNK:u32=128*1024;
pub type Fetch=Arc<dyn Fn(u64,u32)->Result<Vec<u8>,()>+Send+Sync>;
#[derive(Default)]
struct Cache { file:Option<DiskFile>, chunks:BTreeSet<u64>, complete:bool, created:bool }
pub struct File {
    pub name:String,pub size:u64,pub modified:u64,
    cache_path:PathBuf,fetch:Fetch,active:AtomicBool,opens:AtomicUsize,cache:Mutex<Cache>,
}
impl File {
    pub fn new(name:String,size:u64,modified:u64,fetch:Fetch)->Arc<Self> {
        let token=format!("{:032x}",rand::random::<u128>());
        let root=crate::config::path().parent().unwrap_or(Path::new(".")).join("clipboard-cache");
        let cache_path=root.join(format!("{token}.cache"));
        Arc::new(Self{name,size,modified,cache_path,fetch,active:AtomicBool::new(true),opens:AtomicUsize::new(0),cache:Mutex::new(Cache::default())})
    }
    pub fn active(&self)->bool {self.active.load(Ordering::Acquire)}
    pub fn invalidate(&self) {
        self.active.store(false,Ordering::Release);
        if self.opens.load(Ordering::Acquire)==0 {
            if let Ok(mut cache)=self.cache.try_lock() {self.clear(&mut cache);}
        }
    }
    fn clear(&self,cache:&mut Cache) {
        let created=cache.created;cache.file=None;cache.chunks.clear();cache.complete=false;cache.created=false;
        if created{let _=std::fs::remove_file(&self.cache_path);}
    }
    fn open_cache(&self,cache:&mut Cache)->Result<(),()> {
        if cache.file.is_some(){return Ok(());}
        let file=if cache.created {
            std::fs::OpenOptions::new().read(true).write(true).open(&self.cache_path).map_err(|_|())?
        } else {
            let dir=self.cache_path.parent().ok_or(())?;
            std::fs::DirBuilder::new().mode(0o700).recursive(true).create(dir).map_err(|_|())?;
            let file=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&self.cache_path).map_err(|_|())?;
            cache.created=true;
            log::info!("Clipboard file cache started on content read: bytes={}",self.size);file
        };
        cache.file=Some(file);Ok(())
    }
    fn release(&self) {
        if self.opens.fetch_sub(1,Ordering::AcqRel)==1 {
            if let Ok(mut cache)=self.cache.try_lock(){cache.file=None;if !self.active(){self.clear(&mut cache);}}
        }
    }
    pub fn read(&self,offset:u64,size:u32)->Result<Vec<u8>,()> {
        if offset>=self.size {return Ok(Vec::new());}
        if size>1024*1024 {return Err(());}
        let length=u64::from(size).min(self.size-offset);
        if length==0 {return Ok(Vec::new());}
        let mut cache=self.cache.lock().map_err(|_|())?;
        let first=offset/u64::from(CHUNK);let last=(offset+length-1)/u64::from(CHUNK);
        for index in first..=last {
            if cache.chunks.contains(&index) {continue;}
            if !self.active() {return Err(());}
            let start=index*u64::from(CHUNK);let wanted=u64::from(CHUNK).min(self.size-start) as u32;
            let data=(self.fetch)(start,wanted)?;
            if data.len()!=wanted as usize {return Err(());}
            self.open_cache(&mut cache)?;
            cache.file.as_ref().ok_or(())?.write_all_at(&data,start).map_err(|_|())?;
            cache.chunks.insert(index);
        }
        self.open_cache(&mut cache)?;
        let mut data=vec![0;length as usize];cache.file.as_ref().ok_or(())?.read_exact_at(&mut data,offset).map_err(|_|())?;
        if !cache.complete && cache.chunks.len() as u64==self.size.div_ceil(u64::from(CHUNK)) {
            cache.complete=true;log::info!("Clipboard file cache complete: bytes={}",self.size);
        }
        Ok(data)
    }
}
impl Drop for File {fn drop(&mut self){let cache=self.cache.get_mut().unwrap_or_else(|e|e.into_inner());cache.file=None;if cache.created{let _=std::fs::remove_file(&self.cache_path);}}}
pub struct Directory {pub name:String,pub modified:u64}
pub struct Selection {token:String,active:AtomicBool,pub files:Vec<Arc<File>>,pub directories:Vec<Directory>}
impl Selection {
    pub fn new(files:Vec<Arc<File>>,directories:Vec<Directory>)->Arc<Self>{Arc::new(Self{token:format!("{:032x}",rand::random::<u128>()),active:AtomicBool::new(true),files,directories})}
    pub fn invalidate(&self){self.active.store(false,Ordering::Release);for file in &self.files {file.invalidate();}}
}

#[cfg(target_os="linux")]
mod native {
    use super::*;
    use std::{collections::HashMap,ffi::OsStr,sync::OnceLock,time::{Duration,UNIX_EPOCH}};
    use fuser::{Filesystem,Request,INodeNo,FileHandle,FileType,FileAttr,ReplyEntry,ReplyAttr,ReplyOpen,ReplyData,ReplyDirectory,OpenFlags,LockOwner,Errno,FopenFlags};
    enum Kind {Directory{children:std::collections::BTreeMap<String,u64>,modified:u64},File(Arc<File>)}
    struct Entry {parent:u64,kind:Kind}
    struct Entries {next:u64,nodes:HashMap<u64,Entry>,groups:HashMap<String,(u64,Arc<Selection>)>}
    impl Entries {
        fn new()->Self {
            let mut nodes=HashMap::new();nodes.insert(1,Entry{parent:1,kind:Kind::Directory{children:Default::default(),modified:0}});
            Self{next:2,nodes,groups:HashMap::new()}
        }
        fn prune(&mut self) {
            let old:Vec<_>=self.groups.iter().filter(|(_,(_,selection))|!selection.active.load(Ordering::Acquire) && selection.files.iter().all(|f|f.opens.load(Ordering::Acquire)==0)).map(|(name,(root,_))|(name.clone(),*root)).collect();
            for(name,root)in old {
                let mut pending=vec![root];
                while let Some(ino)=pending.pop(){if let Some(entry)=self.nodes.remove(&ino){if let Kind::Directory{children,..}=entry.kind{pending.extend(children.into_values());}}}
                if let Kind::Directory{children,..}=&mut self.nodes.get_mut(&1).unwrap().kind{children.remove(&name);}
                self.groups.remove(&name);
            }
        }
        fn directory(&mut self,parent:u64,name:&str,modified:u64)->Result<u64,&'static str> {
            if let Some(Entry{kind:Kind::Directory{children,..},..})=self.nodes.get(&parent) {
                if let Some(ino)=children.get(name){
                    let ino=*ino;
                    if let Some(Entry{kind:Kind::Directory{modified:time,..},..})=self.nodes.get_mut(&ino){if modified!=0{*time=modified;}return Ok(ino);}
                    return Err("clipboard path collides with a file");
                }
            } else {return Err("clipboard parent is not a directory");}
            let ino=self.next;self.next+=1;
            self.nodes.insert(ino,Entry{parent,kind:Kind::Directory{children:Default::default(),modified}});
            if let Kind::Directory{children,..}=&mut self.nodes.get_mut(&parent).unwrap().kind{children.insert(name.to_owned(),ino);}
            Ok(ino)
        }
        fn parents(&mut self,root:u64,path:&str)->Result<(u64,String),&'static str> {
            let mut parts=path.split('/').peekable();let mut parent=root;
            while let Some(part)=parts.next(){if parts.peek().is_none(){return Ok((parent,part.to_owned()));}parent=self.directory(parent,part,0)?;}
            Err("empty clipboard path")
        }
    }
    struct View(Arc<Mutex<Entries>>);
    struct Mount {path:PathBuf,entries:Arc<Mutex<Entries>>,session:fuser::BackgroundSession}
    static MOUNT:OnceLock<Mutex<Option<Mount>>>=OnceLock::new();
    fn attr(ino:u64,entry:&Entry)->FileAttr {
        let(directory,size,modified)=match &entry.kind{Kind::Directory{modified,..}=>(true,0,*modified),Kind::File(f)=>(false,f.size,f.modified)};
        let time=UNIX_EPOCH.checked_add(Duration::from_secs(modified)).unwrap_or(UNIX_EPOCH);
        FileAttr{ino:INodeNo(ino),size,blocks:size.div_ceil(512),atime:time,mtime:time,ctime:time,crtime:UNIX_EPOCH,
            kind:if directory{FileType::Directory}else{FileType::RegularFile},perm:if directory{0o700}else{0o600},nlink:if directory{2}else{1},
            uid:unsafe{libc::getuid()},gid:unsafe{libc::getgid()},rdev:0,flags:0,blksize:CHUNK}
    }
    impl Filesystem for View {
        fn lookup(&self,_req:&Request,parent:INodeNo,name:&OsStr,reply:ReplyEntry) {
            let entries=self.0.lock().unwrap();let parent=u64::from(parent);
            let found=entries.nodes.get(&parent).and_then(|e|match &e.kind{Kind::Directory{children,..}=>name.to_str().and_then(|n|children.get(n)),_=>None}).and_then(|ino|entries.nodes.get(ino).map(|e|(*ino,e)));
            if let Some((ino,entry))=found{reply.entry(&Duration::from_secs(1),&attr(ino,entry),fuser::Generation(1));}else{reply.error(Errno::ENOENT);}
        }
        fn getattr(&self,_req:&Request,ino:INodeNo,_fh:Option<FileHandle>,reply:ReplyAttr) {
            let entries=self.0.lock().unwrap();let ino=u64::from(ino);
            if let Some(entry)=entries.nodes.get(&ino){reply.attr(&Duration::from_secs(1),&attr(ino,entry));}else{reply.error(Errno::ENOENT);}
        }
        fn open(&self,_req:&Request,ino:INodeNo,flags:OpenFlags,reply:ReplyOpen) {
            if flags.0&libc::O_ACCMODE!=libc::O_RDONLY{reply.error(Errno::EROFS);return;}
            let entries=self.0.lock().unwrap();
            if let Some(Entry{kind:Kind::File(file),..})=entries.nodes.get(&u64::from(ino)){
                if !file.active(){reply.error(Errno::ESTALE);return;}
                file.opens.fetch_add(1,Ordering::AcqRel);reply.opened(FileHandle(u64::from(ino)),FopenFlags::FOPEN_DIRECT_IO);
            }else{reply.error(Errno::ENOENT);}
        }
        fn read(&self,_req:&Request,ino:INodeNo,_fh:FileHandle,offset:u64,size:u32,_flags:OpenFlags,_owner:Option<LockOwner>,reply:ReplyData) {
            let file=self.0.lock().unwrap().nodes.get(&u64::from(ino)).and_then(|e|match &e.kind{Kind::File(f)=>Some(f.clone()),_=>None});
            match file.and_then(|f|f.read(offset,size).ok()){Some(data)=>reply.data(&data),None=>reply.error(Errno::EIO)}
        }
        fn release(&self,_req:&Request,ino:INodeNo,_fh:FileHandle,_flags:OpenFlags,_owner:Option<LockOwner>,_flush:bool,reply:fuser::ReplyEmpty) {
            if let Some(Entry{kind:Kind::File(file),..})=self.0.lock().unwrap().nodes.get(&u64::from(ino)){file.release();}reply.ok();
        }
        fn readdir(&self,_req:&Request,ino:INodeNo,_fh:FileHandle,offset:u64,mut reply:ReplyDirectory) {
            let ino=u64::from(ino);let entries=self.0.lock().unwrap();
            let Some(Entry{parent,kind:Kind::Directory{children,..}})=entries.nodes.get(&ino)else{reply.error(Errno::ENOTDIR);return;};
            let mut list=vec![(ino,FileType::Directory,".".to_owned()),(*parent,FileType::Directory,"..".to_owned())];
            for(name,child)in children{let kind=if matches!(entries.nodes[child].kind,Kind::Directory{..}){FileType::Directory}else{FileType::RegularFile};list.push((*child,kind,name.clone()));}
            for(index,(ino,kind,name))in list.iter().enumerate().skip(offset as usize){if reply.add(INodeNo(*ino),(index+1)as u64,*kind,name){break;}}reply.ok();
        }
    }
    fn start()->io::Result<Mount> {
        let base=std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let root=base.join(format!("uuremote-clipboard-{}",unsafe{libc::getuid()}));
        std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&root)?;
        if std::fs::symlink_metadata(&root)?.file_type().is_symlink(){return Err(io::ErrorKind::PermissionDenied.into());}
        // Recover only this application's orphaned per-process mount directories.
        for entry in std::fs::read_dir(&root)?.flatten() {
            let name=entry.file_name();let Some(name)=name.to_str() else{continue;};
            let Some((pid,token))=name.split_once('-') else{continue;};
            let Some(pid)=pid.parse::<u32>().ok().filter(|p|*p>1) else{continue;};
            if token.len()!=32 || !token.bytes().all(|b|b.is_ascii_hexdigit()) || Path::new(&format!("/proc/{pid}")).exists() {continue;}
            if !entry.file_type().is_ok_and(|t|t.is_dir()) {continue;}
            let _=std::process::Command::new("fusermount3").args(["-u","-z"]).arg(entry.path()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
            let _=std::fs::remove_dir(entry.path());
        }
        let path=root.join(format!("{}-{:032x}",std::process::id(),rand::random::<u128>()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        let entries=Arc::new(Mutex::new(Entries::new()));
        let mut config=fuser::Config::default();config.n_threads=Some(4);
        config.mount_options=vec![fuser::MountOption::RO,fuser::MountOption::DefaultPermissions,fuser::MountOption::FSName("uuremote-clipboard".into())];
        let session=match fuser::spawn_mount(View(entries.clone()),&path,&config){Ok(s)=>s,Err(e)=>{let _=std::fs::remove_dir(&path);return Err(e);}};
        Ok(Mount{path,entries,session})
    }
    pub fn publish(selection:Arc<Selection>)->Result<Vec<PathBuf>,&'static str> {
        let mut current=MOUNT.get_or_init(||Mutex::new(None)).lock().map_err(|_|"file clipboard mount lock failed")?;
        if current.is_none(){*current=Some(start().map_err(|_|"file clipboard requires accessible /dev/fuse and fusermount3")?);}
        let mount=current.as_ref().unwrap();let mut entries=mount.entries.lock().unwrap();entries.prune();
        let root=if let Some((root,_))=entries.groups.get(&selection.token){*root}else{
            let root=entries.directory(1,&selection.token,0)?;
            for directory in &selection.directories{let(parent,name)=entries.parents(root,&directory.name)?;entries.directory(parent,&name,directory.modified)?;}
            for file in &selection.files{
                let(parent,name)=entries.parents(root,&file.name)?;
                let exists=matches!(&entries.nodes[&parent].kind,Kind::Directory{children,..}if children.contains_key(&name));
                if exists{return Err("duplicate clipboard path");}
                let ino=entries.next;entries.next+=1;entries.nodes.insert(ino,Entry{parent,kind:Kind::File(file.clone())});
                if let Kind::Directory{children,..}=&mut entries.nodes.get_mut(&parent).unwrap().kind{children.insert(name,ino);}
            }
            entries.groups.insert(selection.token.clone(),(root,selection.clone()));root
        };
        let Kind::Directory{children,..}=&entries.nodes[&root].kind else{return Err("clipboard root missing");};
        Ok(children.keys().map(|name|mount.path.join(&selection.token).join(name)).collect())
    }
    pub fn shutdown() {
        let Some(current)=MOUNT.get() else{return;};
        let mount=current.lock().unwrap().take();
        if let Some(mount)=mount {
            for entry in mount.entries.lock().unwrap().nodes.values(){if let Kind::File(file)=&entry.kind{file.invalidate();}}
            let path=mount.path;drop(mount.session);let _=std::fs::remove_dir(path);
        }
    }
}
#[cfg(target_os="linux")]
pub use native::{publish,shutdown};
#[cfg(not(target_os="linux"))]
pub fn publish(_:Arc<Selection>)->Result<Vec<PathBuf>,&'static str>{Err("file clipboard virtual files require Linux")}
#[cfg(not(target_os="linux"))]
pub fn shutdown(){}
