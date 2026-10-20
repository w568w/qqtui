use crate::model::Attachment;
use anyhow::{Context, Result, bail};
use base64::Engine;
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType},
    sliced::SlicedProtocol,
};
use std::{
    collections::{HashMap, HashSet},
    io::{BufReader, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc, watch};

#[cfg(target_os = "linux")]
use std::process::Stdio;
#[cfg(target_os = "linux")]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

#[derive(Debug)]
pub struct CacheLease(std::fs::File);
impl CacheLease {
    fn acquire(path: &str, cache: &Path) -> Result<Option<Arc<Self>>> {
        let root = cache.canonicalize().unwrap_or_else(|_| cache.to_path_buf());
        let path = Path::new(path).canonicalize()?;
        if !path.starts_with(root) {
            return Ok(None);
        }
        let file = std::fs::File::open(&path)?;
        fs2::FileExt::try_lock_shared(&file)?;
        Ok(Some(Arc::new(Self(file))))
    }
}
impl Drop for CacheLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}
impl Attachment {
    pub fn pin_cached(mut self, cache: &Path) -> Result<Self> {
        if self.lease.is_none() {
            self.lease = CacheLease::acquire(&self.path, cache)?;
        }
        Ok(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImageKey {
    pub path: String,
    pub width: u16,
    pub height: u16,
    pub upscale: bool,
    pub animate: bool,
}
pub struct ImageResult {
    pub key: ImageKey,
    generation: u64,
    pub outcome: ImageOutcome,
}
pub enum ImageOutcome {
    Ready(CachedImage),
    Frame {
        protocol: SlicedProtocol,
        index: usize,
        kitty_id: u32,
    },
    KittyFrame {
        data: Vec<u8>,
        index: usize,
        kitty_id: u32,
    },
    Failed(String),
    Cancelled,
}
pub struct CachedImage {
    protocol: SlicedProtocol,
    bytes: u64,
    kitty_id: u32,
    displayed: bool,
    animation: Option<Animation>,
    dimensions: (u32, u32),
    used: u64,
    _lease: Option<Arc<CacheLease>>,
}
struct Animation {
    frames: Arc<Vec<(image::DynamicImage, Duration)>>,
    started: Instant,
    index: usize,
    period: Duration,
}
impl Animation {
    fn frame_at(&self, now: Instant) -> usize {
        let mut elapsed =
            now.saturating_duration_since(self.started).as_nanos() % self.period.as_nanos();
        for (index, (_, duration)) in self.frames.iter().enumerate() {
            if elapsed < duration.as_nanos() {
                return index;
            }
            elapsed -= duration.as_nanos();
        }
        0
    }
}
fn image_layout(
    picker: &Picker,
    key: &ImageKey,
    image: &image::DynamicImage,
    source_dimensions: (u32, u32),
) -> (ratatui::layout::Size, Resize) {
    let font = picker.font_size();
    let pixels = f64::from(key.width)
        * f64::from(key.height)
        * f64::from(font.width)
        * f64::from(font.height);
    let scale = (4.0 * 1024.0 * 1024.0 / pixels.max(1.0)).sqrt().min(1.0);
    let size = ratatui::layout::Size::new(
        (f64::from(key.width) * scale).floor().max(1.0) as u16,
        (f64::from(key.height) * scale).floor().max(1.0) as u16,
    );
    let resize = if key.upscale
        || image.width() < source_dimensions.0
        || image.height() < source_dimensions.1
    {
        Resize::Scale(None)
    } else {
        Resize::Fit(None)
    };
    (size, resize)
}

fn encode(
    picker: &Picker,
    key: &ImageKey,
    image: image::DynamicImage,
    kitty_id: u32,
    source_dimensions: (u32, u32),
) -> Result<SlicedProtocol> {
    let (size, resize) = image_layout(picker, key, &image, source_dimensions);
    if picker.protocol_type() == ProtocolType::Kitty {
        let actual = resize.size_for(&image, picker.font_size(), size);
        let image = resize.resize(&image, picker.font_size(), actual, None);
        return Ok(SlicedProtocol::Kitty(
            ratatui_image::protocol::kitty::Kitty::new(
                image,
                actual,
                kitty_id,
                picker.tmux_detected(),
                picker
                    .capabilities()
                    .contains(&ratatui_image::picker::Capability::KittyCompression),
            )?,
        ));
    }
    Ok(SlicedProtocol::new_with_resize(
        picker, image, size, resize,
    )?)
}

fn kitty_frame(
    picker: &Picker,
    key: &ImageKey,
    image: &image::DynamicImage,
    kitty_id: u32,
    source_dimensions: (u32, u32),
) -> Result<Vec<u8>> {
    let (size, resize) = image_layout(picker, key, image, source_dimensions);
    let actual = resize.size_for(image, picker.font_size(), size);
    let image = resize.resize(image, picker.font_size(), actual, None);
    let mut png = std::io::Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
    let (start, escape, end) =
        ratatui_image::picker::cap_parser::Parser::tmux_start_escape_end(picker.tmux_detected());
    let mut output = Vec::new();
    let chunks = encoded.as_bytes().chunks(4096);
    let count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let more = u8::from(index + 1 < count);
        if index == 0 {
            write!(
                output,
                "{start}{escape}_Ga=f,f=100,i={kitty_id},r=1,X=1,q=2,m={more};"
            )?;
        } else {
            write!(output, "{start}{escape}_Ga=f,q=2,m={more};")?;
        }
        output.extend_from_slice(chunk);
        write!(output, "{escape}\\{end}")?;
    }
    write!(
        output,
        "{start}{escape}_Ga=a,i={kitty_id},c=1,q=2{escape}\\{end}"
    )?;
    Ok(output)
}

fn check_animation_canvas(dimensions: (u32, u32)) -> Result<()> {
    if u64::from(dimensions.0) * u64::from(dimensions.1) * 4 > 32 * 1024 * 1024 {
        bail!("动态图画布超过 32 MiB");
    }
    Ok(())
}
fn decode(path: &str, frame_budget: usize) -> Result<Vec<(image::DynamicImage, Duration)>> {
    use image::{AnimationDecoder, ImageDecoder, ImageFormat};
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(32 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    reader.limits(limits.clone());
    let input = || -> Result<_> { Ok(BufReader::new(std::fs::File::open(path)?)) };
    let frames = match reader.format() {
        Some(ImageFormat::Gif) => {
            let mut decoder = image::codecs::gif::GifDecoder::new(input()?)?;
            decoder.set_limits(limits)?;
            check_animation_canvas(decoder.dimensions())?;
            Some(decoder.into_frames())
        }
        Some(ImageFormat::Png) => {
            let mut decoder = image::codecs::png::PngDecoder::new(input()?)?;
            decoder.set_limits(limits)?;
            if decoder.is_apng()? {
                check_animation_canvas(decoder.dimensions())?;
                Some(decoder.apng()?.into_frames())
            } else {
                None
            }
        }
        Some(ImageFormat::WebP) => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(input()?)?;
            decoder.set_limits(limits)?;
            if decoder.has_animation() {
                check_animation_canvas(decoder.dimensions())?;
                Some(decoder.into_frames())
            } else {
                None
            }
        }
        _ => None,
    };
    let Some(frames) = frames else {
        return Ok(vec![(
            reader.decode()?.thumbnail(1024, 1024),
            Duration::ZERO,
        )]);
    };
    let mut images: Vec<(image::DynamicImage, Duration)> = Vec::new();
    let mut edge = 512;
    let mut bytes = 0;
    for frame in frames {
        let frame = frame?;
        if images.len() >= 512 {
            bail!("动态图超过 512 帧");
        }
        let delay = Duration::from(frame.delay());
        let delay = if delay.is_zero() {
            Duration::from_millis(10)
        } else {
            delay
        };
        let mut image = image::DynamicImage::ImageRgba8(frame.into_buffer()).thumbnail(edge, edge);
        while bytes + image.as_bytes().len() > frame_budget && edge > 1 {
            edge = (edge / 2).max(1);
            for (old, _) in &mut images {
                *old = old.thumbnail(edge, edge);
            }
            bytes = images.iter().map(|(old, _)| old.as_bytes().len()).sum();
            image = image.thumbnail(edge, edge);
        }
        bytes += image.as_bytes().len();
        images.push((image, delay));
    }
    if images.is_empty() {
        bail!("动态图没有可用帧");
    }
    Ok(images)
}
pub struct Media {
    pub picker: Picker,
    entries: HashMap<ImageKey, CachedImage>,
    pending: HashMap<ImageKey, u32>,
    failed: HashMap<ImageKey, String>,
    workers: Arc<Semaphore>,
    visible: watch::Sender<HashSet<ImageKey>>,
    pub tx: mpsc::Sender<ImageResult>,
    pub rx: mpsc::Receiver<ImageResult>,
    pub clear_required: bool,
    generation: u64,
    retired: Vec<u32>,
    terminal_output: Vec<u8>,
    next_image_id: u32,
    clock: u64,
    pub owned_cache: Option<std::path::PathBuf>,
    dimensions: HashMap<String, (u32, u32)>,
    deferred: HashSet<ImageKey>,
}
impl Media {
    pub fn new(picker: Picker) -> Self {
        let (tx, rx) = mpsc::channel(8);
        let (visible, _) = watch::channel(HashSet::new());
        Self {
            picker,
            entries: HashMap::new(),
            pending: HashMap::new(),
            failed: HashMap::new(),
            workers: Arc::new(Semaphore::new(2)),
            visible,
            tx,
            rx,
            clear_required: false,
            generation: 0,
            clock: 0,
            owned_cache: None,
            dimensions: HashMap::new(),
            deferred: HashSet::new(),
            retired: Vec::new(),
            terminal_output: Vec::new(),
            next_image_id: u32::from_le_bytes(
                uuid::Uuid::new_v4().as_bytes()[..4].try_into().unwrap(),
            ),
        }
    }
    pub fn image(&mut self, key: &ImageKey) -> Option<&SlicedProtocol> {
        self.clock = self.clock.wrapping_add(1);
        self.entries.get_mut(key).map(|image| {
            image.used = self.clock;
            image.displayed = true;
            &image.protocol
        })
    }
    fn allocate_image_id(&mut self) -> u32 {
        loop {
            self.next_image_id = self.next_image_id.wrapping_add(1);
            let id = self.next_image_id;
            if id != 0
                && !self.pending.values().any(|pending| *pending == id)
                && !self.entries.values().any(|image| image.kitty_id == id)
                && !self.retired.contains(&id)
            {
                return id;
            }
        }
    }
    pub fn set_visible(&mut self, keys: impl IntoIterator<Item = ImageKey>) {
        let keys = keys.into_iter().collect::<HashSet<_>>();
        if *self.visible.borrow() != keys {
            self.deferred.clear();
            self.visible.send_replace(keys);
        }
    }
    pub fn inline_size(&self, a: &Attachment, width: usize) -> (u16, usize) {
        let (w, h) = self
            .dimensions
            .get(&a.path)
            .copied()
            .unwrap_or((a.width, a.height));
        if w == 0 || h == 0 {
            return (width.min(40) as u16, 8);
        }
        let font = self.picker.font_size();
        let scale = ((width.min(40) as f64 * f64::from(font.width)) / f64::from(w))
            .min(8.0 * f64::from(font.height) / f64::from(h))
            .min(1.0);
        (
            (f64::from(w) * scale / f64::from(font.width))
                .ceil()
                .max(1.0) as u16,
            (f64::from(h) * scale / f64::from(font.height))
                .ceil()
                .max(1.0) as usize,
        )
    }
    pub fn failure(&self, key: &ImageKey) -> Option<&str> {
        self.failed.get(key).map(String::as_str)
    }
    pub fn advance(&mut self, now: Instant) {
        let visible = self.visible.borrow().iter().cloned().collect::<Vec<_>>();
        for key in &visible {
            if self.pending.len() >= 8 {
                break;
            }
            let Some(animation) = self
                .entries
                .get(key)
                .and_then(|entry| entry.animation.as_ref())
            else {
                continue;
            };
            let index = animation.frame_at(now);
            if index == animation.index || self.pending.contains_key(key) {
                continue;
            }
            let frames = animation.frames.clone();
            let dimensions = self.entries[key].dimensions;
            let direct = self.picker.protocol_type() == ProtocolType::Kitty;
            if direct && !self.entries[key].displayed {
                continue;
            }
            let kitty_id = if direct {
                self.entries[key].kitty_id
            } else {
                self.allocate_image_id()
            };
            self.pending.insert(key.clone(), kitty_id);
            let key = key.clone();
            let picker = self.picker.clone();
            let workers = self.workers.clone();
            let visible = self.visible.subscribe();
            let tx = self.tx.clone();
            let generation = self.generation;
            tokio::spawn(async move {
                let Ok(permit) = workers.acquire_owned().await else {
                    return;
                };
                let outcome = if !visible.borrow().contains(&key) {
                    ImageOutcome::Cancelled
                } else {
                    let job_key = key.clone();
                    match tokio::task::spawn_blocking(move || {
                        if direct {
                            kitty_frame(&picker, &job_key, &frames[index].0, kitty_id, dimensions)
                                .map(|data| ImageOutcome::KittyFrame {
                                    data,
                                    index,
                                    kitty_id,
                                })
                        } else {
                            encode(
                                &picker,
                                &job_key,
                                frames[index].0.clone(),
                                kitty_id,
                                dimensions,
                            )
                            .map(|protocol| ImageOutcome::Frame {
                                protocol,
                                index,
                                kitty_id,
                            })
                        }
                    })
                    .await
                    {
                        Ok(Ok(outcome)) => outcome,
                        Ok(Err(e)) => ImageOutcome::Failed(e.to_string()),
                        Err(e) => ImageOutcome::Failed(e.to_string()),
                    }
                };
                drop(permit);
                let _ = tx
                    .send(ImageResult {
                        key,
                        generation,
                        outcome,
                    })
                    .await;
            });
        }
    }
    pub fn request(&mut self, key: ImageKey) {
        if self.entries.contains_key(&key)
            || self.pending.contains_key(&key)
            || self.failed.contains_key(&key)
            || self.deferred.contains(&key)
            || self.pending.len() >= 8
        {
            return;
        }
        let kitty_id = self.allocate_image_id();
        self.pending.insert(key.clone(), kitty_id);
        let generation = self.generation;
        let picker = self.picker.clone();
        let workers = self.workers.clone();
        let tx = self.tx.clone();
        let visible = self.visible.subscribe();
        let owned_cache = self.owned_cache.clone();
        tokio::spawn(async move {
            let Ok(permit) = workers.acquire_owned().await else {
                return;
            };
            if !visible.borrow().contains(&key) {
                let _ = tx
                    .send(ImageResult {
                        key,
                        generation,
                        outcome: ImageOutcome::Cancelled,
                    })
                    .await;
                return;
            }
            let job_key = key.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<CachedImage> {
                let lease = owned_cache
                    .as_ref()
                    .map(|cache| CacheLease::acquire(&job_key.path, cache))
                    .transpose()?
                    .flatten();
                let dimensions = image::ImageReader::open(&job_key.path)?
                    .with_guessed_format()?
                    .into_dimensions()?;
                let mut frames = decode(
                    &job_key.path,
                    if job_key.upscale {
                        24 * 1024 * 1024
                    } else {
                        8 * 1024 * 1024
                    },
                )?;
                if !job_key.animate {
                    frames.truncate(1);
                }
                let protocol =
                    encode(&picker, &job_key, frames[0].0.clone(), kitty_id, dimensions)?;
                let font = picker.font_size();
                let encoded_bytes = (u64::from(job_key.width)
                    * u64::from(job_key.height)
                    * u64::from(font.width)
                    * u64::from(font.height))
                .min(4 * 1024 * 1024)
                    * 6;
                let animation = if frames.len() > 1 {
                    Some(Animation {
                        period: frames.iter().map(|(_, delay)| *delay).sum(),
                        frames: Arc::new(frames),
                        started: Instant::now(),
                        index: 0,
                    })
                } else {
                    None
                };
                let bytes = encoded_bytes
                    + animation.as_ref().map_or(0, |animation| {
                        animation
                            .frames
                            .iter()
                            .map(|(image, _)| image.as_bytes().len() as u64)
                            .sum()
                    });
                Ok(CachedImage {
                    protocol,
                    bytes,
                    kitty_id,
                    displayed: false,
                    animation,
                    dimensions,
                    used: 0,
                    _lease: lease,
                })
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|v| v);
            drop(permit);
            let _ = tx
                .send(ImageResult {
                    key,
                    generation,
                    outcome: match result {
                        Ok(p) => ImageOutcome::Ready(p),
                        Err(e) => ImageOutcome::Failed(e.to_string()),
                    },
                })
                .await;
        });
    }
    /// Returns pixel-only terminal output that does not require a UI redraw.
    pub fn complete(&mut self, result: ImageResult) -> bool {
        if result.generation != self.generation {
            return false;
        }
        self.pending.remove(&result.key);
        if !self.visible.borrow().contains(&result.key) {
            return false;
        }
        match result.outcome {
            ImageOutcome::Ready(image) => {
                if self.dimensions.len() >= 512 {
                    self.dimensions
                        .retain(|p, _| self.entries.keys().any(|k| &k.path == p));
                }
                self.dimensions
                    .insert(result.key.path.clone(), image.dimensions);
                while self.entries.len() >= 128
                    || self.entries.values().map(|e| e.bytes).sum::<u64>() + image.bytes
                        > 64 * 1024 * 1024
                {
                    let victim = self
                        .entries
                        .iter()
                        .filter(|(k, _)| !self.visible.borrow().contains(*k))
                        .min_by_key(|(_, v)| v.used)
                        .map(|(k, _)| k.clone());
                    let Some(victim) = victim else {
                        self.deferred.insert(result.key);
                        return true;
                    };
                    if let Some(old) = self.entries.remove(&victim)
                        && old.displayed
                    {
                        self.retired.push(old.kitty_id);
                    }
                }
                self.entries.insert(result.key, image);
            }
            ImageOutcome::Frame {
                protocol,
                index,
                kitty_id,
            } => {
                if let Some(image) = self.entries.get_mut(&result.key)
                    && let Some(animation) = &mut image.animation
                {
                    if image.displayed {
                        self.retired.push(image.kitty_id);
                    }
                    image.displayed = false;
                    image.protocol = protocol;
                    image.kitty_id = kitty_id;
                    animation.index = index;
                }
            }
            ImageOutcome::KittyFrame {
                data,
                index,
                kitty_id,
            } => {
                if let Some(image) = self.entries.get_mut(&result.key)
                    && image.kitty_id == kitty_id
                    && image.displayed
                    && let Some(animation) = &mut image.animation
                {
                    animation.index = index;
                    self.terminal_output.extend(data);
                    return false;
                }
            }
            ImageOutcome::Failed(error) => {
                if let Some(image) = self.entries.get_mut(&result.key) {
                    image.animation = None;
                }
                if self.failed.len() >= 128
                    && let Some(key) = self.failed.keys().next().cloned()
                {
                    self.failed.remove(&key);
                }
                self.failed.insert(result.key, error);
            }
            ImageOutcome::Cancelled => {}
        }
        true
    }
    pub fn has_terminal_output(&self) -> bool {
        !self.terminal_output.is_empty()
    }
    pub fn flush_retired(&mut self) {
        if self.picker.protocol_type() != ProtocolType::Kitty {
            self.terminal_output.clear();
            self.retired.clear();
            return;
        }
        if self.terminal_output.is_empty() && self.retired.is_empty() {
            return;
        }
        let (start, escape, end) = ratatui_image::picker::cap_parser::Parser::tmux_start_escape_end(
            self.picker.tmux_detected(),
        );
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&self.terminal_output);
        self.terminal_output.clear();
        for id in self.retired.drain(..) {
            let _ = write!(out, "{start}{escape}_Ga=d,d=I,i={id},q=2{escape}\\{end}");
        }
        let _ = out.flush();
    }
    pub fn clear_graphics(&mut self) {
        self.retired.extend(
            self.entries
                .values()
                .filter(|image| image.displayed)
                .map(|image| image.kitty_id),
        );
        self.flush_retired();
    }
    pub fn clear_context(&mut self) {
        self.invalidate();
        self.dimensions.clear();
    }
    pub fn invalidate_path(&mut self, path: &str) {
        if self.entries.keys().any(|k| k.path == path)
            || self.pending.keys().any(|k| k.path == path)
        {
            self.invalidate();
        }
    }
    pub fn invalidate(&mut self) {
        self.clear_graphics();
        self.visible.send_replace(HashSet::new());
        self.generation += 1;
        self.pending.clear();
        self.entries.clear();
        self.deferred.clear();
        self.failed.clear();
        self.clear_required = true;
    }
}
#[derive(Clone, Default)]
pub struct ClipboardAccess {
    #[cfg(windows)]
    handle: Arc<std::sync::Mutex<Option<arboard::Clipboard>>>,
}
impl ClipboardAccess {
    pub async fn copy(&self, text: &str) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            linux_copy(text).await
        }
        #[cfg(windows)]
        {
            let handle = self.handle.clone();
            let text = text.to_owned();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let mut handle = handle
                    .lock()
                    .map_err(|_| anyhow::anyhow!("剪贴板服务已退出"))?;
                if handle.is_none() {
                    *handle = Some(arboard::Clipboard::new()?);
                }
                handle.as_mut().unwrap().set_text(text)?;
                Ok(())
            })
            .await?
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = text;
            bail!("此平台尚未提供剪贴板适配器")
        }
    }
    pub async fn paste(&self, cache: &Path) -> Result<Clipboard> {
        #[cfg(target_os = "linux")]
        {
            linux_paste(cache).await
        }
        #[cfg(windows)]
        {
            enum Content {
                Text(String),
                Png(Vec<u8>),
            }
            let handle = self.handle.clone();
            let content = tokio::task::spawn_blocking(move || -> Result<Content> {
                let mut handle = handle
                    .lock()
                    .map_err(|_| anyhow::anyhow!("剪贴板服务已退出"))?;
                if handle.is_none() {
                    *handle = Some(arboard::Clipboard::new()?);
                }
                let clipboard = handle.as_mut().unwrap();
                if let Ok(image) = clipboard.get_image() {
                    if image
                        .width
                        .checked_mul(image.height)
                        .and_then(|v| v.checked_mul(4))
                        .is_none_or(|size| size > 32 * 1024 * 1024)
                    {
                        bail!("剪贴板图片过大");
                    }
                    let rgba = image::RgbaImage::from_raw(
                        image.width as u32,
                        image.height as u32,
                        image.bytes.into_owned(),
                    )
                    .context("剪贴板图片格式无效")?;
                    let mut bytes = std::io::Cursor::new(Vec::new());
                    rgba.write_to(&mut bytes, image::ImageFormat::Png)?;
                    Ok(Content::Png(bytes.into_inner()))
                } else {
                    Ok(Content::Text(clipboard.get_text()?))
                }
            })
            .await??;
            match content {
                Content::Text(text) => Ok(Clipboard::Text(crate::model::safe_text(&text))),
                Content::Png(bytes) => {
                    let file = cache.join(format!("clipboard-{}.png", uuid::Uuid::new_v4()));
                    crate::runtime::private_write(&file, bytes)?;
                    Ok(Clipboard::Attachments(vec![
                        attachment(&file).await?.pin_cached(cache)?,
                    ]))
                }
            }
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = cache;
            bail!("此平台尚未提供剪贴板适配器")
        }
    }
}
pub enum Clipboard {
    Text(String),
    Attachments(Vec<Attachment>),
}
#[cfg(target_os = "linux")]
async fn output_limit(mut command: Command, limit: u64) -> Result<Vec<u8>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut output = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        child
            .stdout
            .take()
            .context("clipboard stdout missing")?
            .take(limit + 1)
            .read_to_end(&mut output),
    )
    .await??;
    if output.len() as u64 > limit {
        bail!("剪贴板内容过大");
    }
    if !child.wait().await?.success() {
        bail!("读取剪贴板失败");
    }
    Ok(output)
}
#[cfg(target_os = "linux")]
fn clipboard_read(target: &str) -> Command {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut cmd = Command::new("wl-paste");
        if target == "TARGETS" {
            cmd.arg("--list-types");
        } else {
            cmd.args(["--type", target]);
        }
        cmd
    } else {
        let mut cmd = Command::new("xclip");
        cmd.args(["-selection", "clipboard", "-o", "-t", target]);
        cmd
    }
}
#[cfg(target_os = "linux")]
async fn linux_paste(cache: &Path) -> Result<Clipboard> {
    let types = clipboard_read("TARGETS");
    let types = String::from_utf8(output_limit(types, 65536).await?)?;
    if types.lines().any(|s| s == "image/png") {
        let cmd = clipboard_read("image/png");
        let bytes = output_limit(cmd, 32 * 1024 * 1024).await?;
        let file = cache.join(format!("clipboard-{}.png", uuid::Uuid::new_v4()));
        crate::runtime::private_write(&file, bytes)?;
        return Ok(Clipboard::Attachments(vec![
            attachment(&file).await?.pin_cached(cache)?,
        ]));
    }
    if types.lines().any(|s| s == "text/uri-list") {
        let cmd = clipboard_read("text/uri-list");
        let text = String::from_utf8(output_limit(cmd, 1024 * 1024).await?)?;
        let mut files = Vec::new();
        for line in text
            .lines()
            .filter(|s| !s.starts_with('#') && !s.is_empty())
        {
            let file = url::Url::parse(line)?
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("剪贴板包含非本地文件"))?;
            files.push(attachment(&file).await?.pin_cached(cache)?);
        }
        return Ok(Clipboard::Attachments(files));
    }
    let cmd = clipboard_read(if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "text/plain;charset=utf-8"
    } else {
        "UTF8_STRING"
    });
    Ok(Clipboard::Text(crate::model::safe_text(
        &String::from_utf8(output_limit(cmd, 1024 * 1024).await?)?,
    )))
}
pub async fn attachment(path: &Path) -> Result<Attachment> {
    let path = path.canonicalize()?;
    tokio::task::spawn_blocking(move || {
        let metadata = path.metadata()?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 1024 * 1024 * 1024 {
            bail!("附件大小必须为 1 B 至 1 GiB");
        }
        let reader = image::ImageReader::open(&path)
            .ok()
            .and_then(|reader| reader.with_guessed_format().ok());
        let format = reader.as_ref().and_then(|reader| reader.format());
        let dimensions = reader.and_then(|reader| reader.into_dimensions().ok());
        let mut name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if dimensions.is_some()
            && let Some(format) = format
        {
            let extension = path.extension().map(|value| value.to_string_lossy());
            if !extension.as_ref().is_some_and(|value| {
                format
                    .extensions_str()
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(value))
            }) {
                let mut filename = std::path::PathBuf::from(&name);
                filename.set_extension(format.extensions_str()[0]);
                name = filename.to_string_lossy().into_owned();
            }
        }
        Ok(Attachment {
            element_id: String::new(),
            kind: if dimensions.is_some() {
                "image"
            } else {
                "file"
            }
            .into(),
            name,
            display_name: None,
            warning: None,
            path: path.to_string_lossy().into(),
            size: metadata.len().to_string(),
            width: dimensions.map(|d| d.0).unwrap_or(0),
            height: dimensions.map(|d| d.1).unwrap_or(0),
            lease: None,
        })
    })
    .await?
}
pub async fn sticker_attachment(path: &Path, display_name: Option<String>) -> Result<Attachment> {
    let mut attachment = attachment(path).await?;
    if attachment.width == 0 || attachment.height == 0 {
        bail!("请选择可解码的图片表情文件");
    }
    attachment.kind = "sticker".into();
    attachment.display_name = display_name;
    Ok(attachment)
}
#[cfg(target_os = "linux")]
async fn linux_copy(text: &str) -> Result<()> {
    let mut cmd = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut cmd = Command::new("wl-copy");
        cmd.args(["--type", "text/plain;charset=utf-8"]);
        cmd
    } else {
        let mut cmd = Command::new("xclip");
        cmd.args(["-selection", "clipboard", "-i", "-t", "UTF8_STRING"]);
        cmd
    };
    let mut child = cmd.stdin(Stdio::piped()).kill_on_drop(true).spawn()?;
    child
        .stdin
        .take()
        .context("clipboard stdin missing")?
        .write_all(text.as_bytes())
        .await?;
    if !child.wait().await?.success() {
        bail!("复制失败");
    }
    Ok(())
}
pub async fn open(path: &str) -> Result<()> {
    if !Path::new(path).is_absolute() {
        let url = url::Url::parse(path)?;
        if !["https", "http", "file"].contains(&url.scheme()) {
            bail!("不支持的链接类型");
        }
    }
    crate::platform::open(path.to_owned()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn sticker_format_comes_from_image_bytes_even_when_qq_uses_no_or_wrong_extension() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["cache-id", "cache-id.jpg"] {
            let file = temp.path().join(name);
            image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 255, 0, 255]))
                .save_with_format(&file, image::ImageFormat::Png)
                .unwrap();
            let attachment = attachment(&file).await.unwrap();
            assert!(attachment.name.ends_with(".png"));
            assert_eq!(attachment.kind, "image");
            assert_eq!((attachment.width, attachment.height), (2, 2));
        }
    }
    #[tokio::test]
    async fn image_work_is_bounded_and_produces_a_fitting_protocol() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("preview.png");
        image::RgbaImage::from_pixel(960, 1707, image::Rgba([20, 120, 220, 255]))
            .save(&file)
            .unwrap();
        let mut media = Media::new(Picker::halfblocks());
        let key = ImageKey {
            path: file.to_string_lossy().into(),
            width: 40,
            height: 8,
            upscale: false,
            animate: true,
        };
        media.set_visible([key.clone()]);
        media.request(key.clone());
        media.request(key.clone());
        assert_eq!(media.pending.len(), 1);
        let result = tokio::time::timeout(Duration::from_secs(5), media.rx.recv())
            .await
            .unwrap()
            .unwrap();
        media.complete(result);
        let protocol = media.image(&key).unwrap();
        assert!(protocol.size().width <= 40);
        assert!(protocol.size().height <= 8);
    }
}

#[cfg(test)]
mod animation_tests {
    #[test]
    fn downsampled_frames_fill_the_original_layout_without_enlarging_small_source_images() {
        let picker = Picker::halfblocks();
        let font = picker.font_size();
        let key = ImageKey {
            path: String::new(),
            width: 12,
            height: 6,
            upscale: false,
            animate: true,
        };
        let frame =
            image::DynamicImage::new_rgba8(u32::from(font.width) * 6, u32::from(font.height) * 3);
        for (columns, rows) in [(12, 6), (6, 3)] {
            let source = (
                u32::from(font.width) * u32::from(columns),
                u32::from(font.height) * u32::from(rows),
            );
            assert_eq!(
                encode(&picker, &key, frame.clone(), 1, source)
                    .unwrap()
                    .size(),
                ratatui::layout::Size::new(columns, rows)
            );
        }
    }
    #[test]
    fn graphics_ids_do_not_reuse_active_pending_or_retired_images_at_wraparound() {
        let mut media = Media::new(Picker::halfblocks());
        let key = ImageKey {
            path: String::new(),
            width: 1,
            height: 1,
            upscale: false,
            animate: true,
        };
        media.next_image_id = u32::MAX;
        media.pending.insert(key, 1);
        media.retired.push(2);
        assert_eq!(media.allocate_image_id(), 3);
    }
    use super::*;
    use image::{
        Delay, Frame, Rgba, RgbaImage,
        codecs::gif::{GifEncoder, Repeat},
    };
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
    use ratatui_image::sliced::{SignedPosition, SlicedImage};

    #[test]
    fn apng_and_webp_decode_all_frames_with_their_delays() {
        for suffix in ["png", "webp"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("tests/fixtures/animation.{suffix}"));
            let frames = decode(path.to_str().unwrap(), 8 * 1024 * 1024).unwrap();
            assert_eq!(frames.len(), 2);
            assert_eq!(frames[0].0.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
            let green = frames[1].0.to_rgba8().get_pixel(0, 0).0;
            assert!(green[0] < 3 && green[1] > 250 && green[2] < 3 && green[3] == 255);
            assert_eq!(frames[1].1, Duration::from_millis(100));
        }
    }
    #[tokio::test]
    async fn animated_frames_loop_only_when_visible_and_survive_no_stale_resize_result() {
        let temp = tempfile::tempdir().unwrap();
        // Magic bytes, rather than the QQ attachment's suffix, determine the decoder.
        let path = temp.path().join("sticker.jpg");
        let mut encoder = GifEncoder::new(std::fs::File::create(&path).unwrap());
        encoder.set_repeat(Repeat::Infinite).unwrap();
        for color in [[255, 0, 0, 255], [0, 255, 0, 255]] {
            encoder
                .encode_frame(Frame::from_parts(
                    RgbaImage::from_fn(64, 64, |x, y| {
                        if x == 0 && y == 0 {
                            Rgba(color)
                        } else {
                            Rgba([
                                ((x * 17 + y * 31) % 255) as u8,
                                ((x * 7 + y * 13) % 255) as u8,
                                color[2],
                                255,
                            ])
                        }
                    }),
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
        }
        drop(encoder);
        let frames = decode(path.to_str().unwrap(), 8 * 1024 * 1024).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].1, Duration::from_millis(100));
        let key = ImageKey {
            path: path.to_string_lossy().into(),
            width: 8,
            height: 4,
            upscale: true,
            animate: true,
        };
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let mut media = Media::new(picker);
        media.set_visible([key.clone()]);
        media.request(key.clone());
        let result = media.rx.recv().await.unwrap();
        media.complete(result);
        let animation = media.entries[&key].animation.as_ref().unwrap();
        let started = animation.started;
        assert_eq!(animation.frame_at(started + Duration::from_millis(250)), 0);
        let render = |media: &mut Media| {
            let area = Rect::new(0, 0, 8, 4);
            let mut buffer = Buffer::empty(area);
            SlicedImage::new(media.image(&key).unwrap(), SignedPosition::from((0, 0)))
                .render(area, &mut buffer);
            buffer[(0, 0)].symbol().to_string()
        };
        let first = render(&mut media);
        let id = media.entries[&key].kitty_id;
        media.advance(started + Duration::from_millis(150));
        let result = tokio::time::timeout(Duration::from_secs(3), media.rx.recv())
            .await
            .unwrap()
            .unwrap();
        media.complete(result);
        let update = String::from_utf8(media.terminal_output.clone()).unwrap();
        let second = render(&mut media);
        assert_ne!(first, second);
        assert!(first.contains(&format!("i={id},")));
        assert!(
            update.starts_with(&format!("\x1b_Ga=f,f=100,i={id},r=1,X=1,")),
            "frame edits must replace pixels, not alpha blend over the previous frame"
        );
        assert!(
            update.matches("_Ga=f").count() > 1,
            "large frame must exercise Kitty continuation chunks"
        );
        assert!(
            !update.contains("\x1b["),
            "Pixel updates must not move or hide the text cursor"
        );
        let payload = update
            .split("\x1b\\")
            .filter_map(|part| part.split_once(';'))
            .filter(|(header, _)| header.contains("_Ga=f"))
            .map(|(_, payload)| payload)
            .collect::<String>();
        let png = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        let frame = image::load_from_memory(&png).unwrap().to_rgba8();
        assert!(!frame.is_empty());
        assert_eq!(media.entries[&key].kitty_id, id);
        assert!(media.retired.is_empty());
        assert_eq!(media.entries[&key].animation.as_ref().unwrap().index, 1);
        media.set_visible([]);
        media.advance(started + Duration::from_millis(250));
        assert!(media.pending.is_empty());
        media.set_visible([key.clone()]);
        media.advance(started + Duration::from_millis(250));
        let stale = media.rx.recv().await.unwrap();
        media.invalidate();
        media.set_visible([key.clone()]);
        media.complete(stale);
        assert!(media.image(&key).is_none());
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[tokio::test]
    async fn twenty_five_visible_emoji_stabilize_without_whole_cache_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("small.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]))
            .save(&file)
            .unwrap();
        let mut media = Media::new(Picker::halfblocks());
        let keys = (1..=25)
            .map(|width| ImageKey {
                path: file.to_string_lossy().into(),
                width,
                height: 2,
                upscale: false,
                animate: false,
            })
            .collect::<Vec<_>>();
        media.set_visible(keys.iter().cloned());
        for _ in 0..25 {
            for key in &keys {
                media.request(key.clone());
            }
            if media.pending.is_empty() {
                break;
            }
            let result = media.rx.recv().await.unwrap();
            media.complete(result);
        }
        while !media.pending.is_empty() {
            let result = media.rx.recv().await.unwrap();
            media.complete(result);
        }
        assert_eq!(media.entries.len(), 25);
        for _ in 0..3 {
            for key in &keys {
                media.request(key.clone());
            }
        }
        assert!(media.pending.is_empty());
        assert!(!media.clear_required);
    }
    #[test]
    fn inline_height_matches_pixels_instead_of_a_fixed_eight_rows() {
        let media = Media::new(Picker::halfblocks());
        let font = media.picker.font_size();
        let a = Attachment {
            width: u32::from(font.width) * 3,
            height: u32::from(font.height),
            ..Default::default()
        };
        assert_eq!(media.inline_size(&a, 40), (3, 1));
    }
}

// Persistent references and live leases protect assets across multiple frontend windows.
pub fn prune_cache(cache: &Path, pins: HashSet<String>) -> Result<()> {
    let cache = cache.canonicalize()?;
    let pins = pins
        .into_iter()
        .map(|p| {
            let p = std::path::PathBuf::from(p);
            p.canonicalize().unwrap_or(p)
        })
        .collect::<HashSet<_>>();
    for (relative, count_limit) in [
        ("", 256usize),
        ("stickers", 256),
        ("market-faces", 256),
        ("forwarded-images", 64),
    ] {
        let dir = cache.join(relative);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if relative.is_empty() && !e.file_name().to_string_lossy().starts_with("clipboard-")
                {
                    return None;
                }
                let metadata = std::fs::symlink_metadata(&path).ok()?;
                if !metadata.is_file() {
                    return None;
                }
                Some((path, metadata.len(), metadata.modified().ok()?))
            })
            .collect::<Vec<_>>();
        files.sort_by_key(|(_, _, time)| *time);
        let mut count = files.len();
        let mut bytes = files.iter().map(|(_, size, _)| size).sum::<u64>();
        for (path, size, time) in files {
            if count <= count_limit && bytes <= 128 * 1024 * 1024 {
                break;
            }
            if pins.contains(&path) || time.elapsed().unwrap_or_default() < Duration::from_secs(60)
            {
                continue;
            }
            let Ok(guard) = std::fs::File::open(&path) else {
                continue;
            };
            if fs2::FileExt::try_lock_exclusive(&guard).is_err() {
                continue;
            }
            if std::fs::remove_file(path).is_ok() {
                count -= 1;
                bytes = bytes.saturating_sub(size);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod cache_ownership_tests {
    use super::*;
    #[test]
    fn cache_cleaning_respects_shared_live_leases_and_persistent_references() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("forwarded-images");
        std::fs::create_dir(&dir).unwrap();
        for i in 0..65 {
            let path = dir.join(format!("{i:032x}"));
            std::fs::write(&path, b"image").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(i)),
                )
                .unwrap();
        }
        let first = dir.join(format!("{:032x}", 0));
        let attachment = Attachment {
            path: first.to_string_lossy().into(),
            ..Default::default()
        }
        .pin_cached(temp.path())
        .unwrap();
        prune_cache(temp.path(), HashSet::new()).unwrap();
        assert!(first.exists());
        assert!(!dir.join(format!("{:032x}", 1)).exists());
        drop(attachment);
        std::fs::write(dir.join("extra"), b"image").unwrap();
        prune_cache(
            temp.path(),
            [first.to_string_lossy().into()].into_iter().collect(),
        )
        .unwrap();
        assert!(first.exists());
        std::fs::write(dir.join("another"), b"image").unwrap();
        prune_cache(temp.path(), HashSet::new()).unwrap();
        assert!(!first.exists());
    }
}
