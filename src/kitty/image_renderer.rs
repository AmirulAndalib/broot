use {
    crate::{
        display::{
            cell_size_in_pixels,
            W,
        },
        errors::ProgramError,
    },
    base64,
    crossterm::{
        cursor,
        QueueableCommand,
    },
    image::{
        DynamicImage,
        GenericImageView,
        RgbImage,
        RgbaImage,
    },
    log::*,
    std::{
        env,
        io::{self, Write},
    },
    tempfile,
    termimad::Area,
};

pub type KittyImageSet = Vec<usize>;

/// How to send the image to kitty
///
/// Note that I didn't test yet the named shared memory
/// solution offered by kitty.
pub enum TransmissionMedium {
    /// write a temp file, then give its path to kitty
    /// in the payload of the escape sequence. It's quite
    /// fast on SSD but a big downside is that it doesn't
    /// work if you're distant
    TempFile,
    /// send the whole rgb or rgba data, encoded in base64,
    /// in the payloads of several escape sequence (each one
    /// containing at most 4096 bytes). Works if broot runs
    /// on remote.
    Chunks,
}

enum ImageData<'i> {
    RgbRef(&'i RgbImage),
    RgbaRef(&'i RgbaImage),
    Rgb(RgbImage),
}
impl<'i> From<&'i DynamicImage> for ImageData<'i> {
    fn from(img: &'i DynamicImage) -> Self {
        if let Some(rgb) = img.as_rgb8() {
            debug!("using rgb");
            Self::RgbRef(rgb)
        } else if let Some(rgba) = img.as_rgba8() {
            debug!("using rgba");
            Self::RgbaRef(rgba)
        } else {
            debug!("converting to rgb8");
            Self::Rgb(img.to_rgb8())
        }
    }
}
impl<'i> ImageData<'i> {
    fn kitty_format(&self) -> &'static str {
        match self {
            Self::RgbaRef(_) => "32",
            _ => "24",
        }
    }
    fn bytes(&self) -> &[u8] {
        match self {
            Self::RgbRef(img) => img.as_raw(),
            Self::RgbaRef(img) => img.as_raw(),
            Self::Rgb(img) => img.as_raw(),
        }
    }
}

/// The max size of a data payload in a kitty escape sequence
/// according to kitty's documentation
const CHUNK_SIZE: usize = 4096;

/// until I'm told there's another terminal supporting the kitty
/// terminal, I think I can just check the name
pub fn is_term_kitty() -> bool {
    if let Ok(term_name) = env::var("TERM") {
        if term_name.contains("kitty") {
            return true;
        }
    }
    false
}

fn div_ceil(a: u32, b: u32) -> u32 {
    a / b + (0 != a % b) as u32
}

/// the image renderer, with knowledge of the
/// console cells dimensions, and built only on Kitty.
///
pub struct KittyImageRenderer {
    cell_width: u32,
    cell_height: u32,
    next_id: usize,
    current_images: Option<KittyImageSet>,
    pub transmission_medium: TransmissionMedium,
}

/// An image prepared for a precise area on screen
///
struct KittyImage<'i> {
    id: usize,
    data: ImageData<'i>,
    img_width: u32,
    img_height: u32,
    area: Area,
}
impl<'i> KittyImage<'i> {
    fn new<'r>(
        src: &'i DynamicImage,
        available_area: &Area,
        renderer: &'r mut KittyImageRenderer,
    ) -> Self {
        let (img_width, img_height) = src.dimensions();
        let area = renderer.rendering_area(img_width, img_height, available_area);
        let data = src.into();
        let id = renderer.new_id();
        Self {
            id,
            data,
            img_width,
            img_height,
            area,
        }
    }
    /// render the image by sending multiple kitty escape sequence, each
    /// one with part of the image raw data (encoded as base64)
    fn print_with_chunks(
        &self,
        w: &mut W,
    ) -> Result<(), ProgramError> {
        let encoded = base64::encode(self.data.bytes());
        w.queue(cursor::MoveTo(self.area.left, self.area.top))?;
        let mut pos = 0;
        loop {
            if pos + CHUNK_SIZE < encoded.len() {
                write!(
                    w,
                    "\u{1b}_Ga=T,f={},t=d,i={},s={},v={},c={},r={},m=1;{}\u{1b}\\",
                    self.data.kitty_format(),
                    self.id,
                    self.img_width,
                    self.img_height,
                    self.area.width,
                    self.area.height,
                    &encoded[pos..pos + CHUNK_SIZE],
                )?;
                pos += CHUNK_SIZE;
            } else {
                // last chunk
                write!(w, "\u{1b}_Gm=0;{}\u{1b}\\", &encoded[pos..encoded.len()],)?;
                break;
            }
        }
        Ok(())
    }
    /// render the image by writing the raw data in a temporary file
    /// then giving to kitty the path to this file in the payload of
    /// a unique kitty ecape sequence
    pub fn print_with_temp_file(
        &self,
        w: &mut W,
    ) -> Result<(), ProgramError> {
        let (mut temp_file, path) = tempfile::Builder::new()
            .prefix("broot-img-preview")
            .tempfile()?
            .keep()
            .map_err(|_| io::Error::new(
                io::ErrorKind::Other,
                "temp file can't be kept",
            ))?;
        temp_file.write_all(self.data.bytes())?;
        temp_file.flush()?;
        let path = path.to_str()
            .ok_or_else(|| io::Error::new(
                io::ErrorKind::Other,
                "Path can't be converted to UTF8",
            ))?;
        let encoded_path = base64::encode(path);
        debug!("temp file written: {:?}", path);
        w.queue(cursor::MoveTo(self.area.left, self.area.top))?;
        write!(
            w,
            "\u{1b}_Ga=T,f={},t=t,i={},s={},v={},c={},r={};{}\u{1b}\\",
            self.data.kitty_format(),
            self.id,
            self.img_width,
            self.img_height,
            self.area.width,
            self.area.height,
            encoded_path,
        )?;
        debug!("file len: {}", temp_file.metadata().unwrap().len());
        Ok(())
    }
}

impl KittyImageRenderer {
    pub fn new() -> Option<Self> {
        if !is_term_kitty() {
            return None;
        }
        cell_size_in_pixels()
            .ok()
            .map(|(cell_width, cell_height)| Self {
                cell_width,
                cell_height,
                current_images: None,
                next_id: 1,
                transmission_medium: TransmissionMedium::Chunks,
            })
    }
    pub fn take_current_images(&mut self) -> Option<KittyImageSet> {
        self.current_images.take()
    }
    /// return a new image id which is assumed will be used
    fn new_id(&mut self) -> usize {
        let new_id = self.next_id;
        self.next_id += 1;
        self.current_images
            .get_or_insert_with(Vec::new)
            .push(new_id);
        new_id
    }
    pub fn print(
        &mut self,
        w: &mut W,
        src: &DynamicImage,
        area: &Area,
    ) -> Result<(), ProgramError> {
        let img = KittyImage::new(src, area, self);
        match self.transmission_medium {
            TransmissionMedium::TempFile => img.print_with_temp_file(w),
            TransmissionMedium::Chunks => img.print_with_chunks(w),
        }
    }
    pub fn erase(
        &mut self,
        w: &mut W,
        ids: KittyImageSet,
    ) -> Result<(), ProgramError> {
        for id in ids {
            debug!("erase kitty image {}", id);
            write!(w, "\u{1b}_Ga=d,d=I,i={}\u{1b}\\", id)?;
        }
        Ok(())
    }
    /// erase all kitty images, even the forgetted ones
    pub fn erase_all(
        &mut self,
        w: &mut W,
    ) -> Result<(), ProgramError> {
        write!(w, "\u{1b}_Ga=d,d=A\u{1b}\\")?;
        self.current_images = None;
        Ok(())
    }
    fn rendering_area(
        &self,
        img_width: u32,
        img_height: u32,
        area: &Area,
    ) -> Area {
        let area_cols: u32 = area.width.into();
        let area_rows: u32 = area.height.into();
        let rdim = self.rendering_dim(img_width, img_height, area_cols, area_rows);
        Area::new(
            area.left + ((area_cols - rdim.0) / 2) as u16,
            area.top + ((area_rows - rdim.1) / 2) as u16,
            rdim.0 as u16,
            rdim.1 as u16,
        )
    }
    fn rendering_dim(
        &self,
        img_width: u32,
        img_height: u32,
        area_cols: u32,
        area_rows: u32,
    ) -> (u32, u32) {
        let optimal_cols = div_ceil(img_width, self.cell_width);
        let optimal_rows = div_ceil(img_height, self.cell_height);
        debug!("area: {:?}", (area_cols, area_rows));
        debug!("optimal: {:?}", (optimal_cols, optimal_rows));
        if optimal_cols <= area_cols && optimal_rows <= area_rows {
            // no constraint (TODO center?)
            (optimal_cols, optimal_rows)
        } else if optimal_cols * area_rows > optimal_rows * area_cols {
            // we're constrained in width
            debug!("constrained in width");
            (area_cols, optimal_rows * area_cols / optimal_cols)
        } else {
            // we're constrained in height
            debug!("constrained in height");
            (optimal_cols * area_rows / optimal_rows, area_rows)
        }
    }
}

