use anyhow::{bail, Context, Result};
use image::{
    codecs::{
        jpeg::JpegEncoder,
        png::PngEncoder,
        webp::WebPEncoder,
    },
    imageops::FilterType,
    ColorType,
    DynamicImage,
    GenericImageView,
    ImageEncoder,
    ImageReader,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars,
    tool,
    tool_handler,
    tool_router,
    ServerHandler,
    ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    ffi::OsString,
    fs,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::Arc,
};

//
// -----------------------------------------------------------------------------
// Configuration
// -----------------------------------------------------------------------------

#[derive(Clone)]
struct ImageServer {
    workspace: Arc<PathBuf>,
    tool_router: ToolRouter<Self>,
}

impl ImageServer {
    fn new(workspace: PathBuf) -> Result<Self> {
        let workspace = fs::canonicalize(&workspace)
            .with_context(|| format!("Workspace does not exist: {}", workspace.display()))?;

        if !workspace.is_dir() {
            bail!("Workspace is not a directory: {}", workspace.display());
        }

        Ok(Self {
            workspace: Arc::new(workspace),
            tool_router: Self::tool_router(),
        })
    }

    /// Joins `path` onto the workspace (or takes it as-is if already absolute).
    /// The result may not exist yet and may still escape the workspace;
    /// callers must run it through `verified_ancestor` before touching disk.
    fn to_absolute(&self, path: &str) -> PathBuf {
        let path = PathBuf::from(path);

        if path.is_absolute() {
            path
        } else {
            self.workspace.join(path)
        }
    }

    /// Walks upward from `absolute` to the nearest path component that
    /// already exists on disk (checked with `symlink_metadata`, so an
    /// existing symlink counts even if its target is missing), fully
    /// resolves that ancestor with `canonicalize`, and verifies it sits
    /// inside the workspace *before* any component is created.
    ///
    /// Returns the canonical ancestor plus the list of components (root to
    /// leaf) that don't exist yet. This ordering is what keeps path
    /// traversal (`../../etc`) and symlink swaps from creating or writing
    /// anything outside the workspace: nothing is created until the
    /// deepest real, verified ancestor is known.
    fn verified_ancestor(&self, absolute: &Path) -> Result<(PathBuf, Vec<OsString>)> {
        let mut existing = absolute.to_path_buf();
        let mut missing = Vec::new();

        while fs::symlink_metadata(&existing).is_err() {
            let name = existing
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("Invalid path: {}", absolute.display()))?
                .to_owned();

            if name == ".." || name == "." {
                bail!(
                    "Path '{}' contains an unresolved traversal component",
                    absolute.display()
                );
            }

            missing.push(name);

            existing = existing
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Invalid path: {}", absolute.display()))?
                .to_path_buf();
        }

        let canonical = fs::canonicalize(&existing)
            .with_context(|| format!("Unable to resolve path: {}", existing.display()))?;

        self.ensure_inside_workspace(&canonical)?;

        missing.reverse();

        Ok((canonical, missing))
    }

    /// Creates `absolute` (and any missing parents) as a directory, then
    /// re-verifies the result is inside the workspace. Only ever called
    /// with a path whose nearest existing ancestor has already been
    /// verified by `verified_ancestor`.
    fn ensure_directory(&self, absolute: PathBuf) -> Result<PathBuf> {
        fs::create_dir_all(&absolute)
            .with_context(|| format!("Unable to create directory: {}", absolute.display()))?;

        let canonical = fs::canonicalize(&absolute)
            .with_context(|| format!("Unable to resolve directory: {}", absolute.display()))?;

        self.ensure_inside_workspace(&canonical)?;

        Ok(canonical)
    }

    fn resolve_input(&self, path: &str) -> Result<PathBuf> {
        let absolute = self.to_absolute(path);
        let (canonical, missing) = self.verified_ancestor(&absolute)?;

        if !missing.is_empty() {
            bail!("Input file does not exist: {}", absolute.display());
        }

        if !canonical.is_file() {
            bail!("Not a file: {}", canonical.display());
        }

        Ok(canonical)
    }

    fn resolve_input_directory(&self, path: &str) -> Result<PathBuf> {
        let absolute = self.to_absolute(path);
        let (canonical, missing) = self.verified_ancestor(&absolute)?;

        if !missing.is_empty() {
            bail!("Directory does not exist: {}", absolute.display());
        }

        if !canonical.is_dir() {
            bail!("Not a directory: {}", canonical.display());
        }

        Ok(canonical)
    }

    fn resolve_output(&self, path: &str) -> Result<PathBuf> {
        let absolute = self.to_absolute(path);
        let (canonical_ancestor, missing) = self.verified_ancestor(&absolute)?;

        if missing.is_empty() {
            if canonical_ancestor.is_dir() {
                bail!("Output path is a directory: {}", canonical_ancestor.display());
            }

            return Ok(canonical_ancestor);
        }

        let filename = missing.last().expect("missing is non-empty").clone();
        let dirs = &missing[..missing.len() - 1];

        let mut target_dir = canonical_ancestor;

        for component in dirs {
            target_dir.push(component);
        }

        if !dirs.is_empty() {
            target_dir = self.ensure_directory(target_dir)?;
        }

        Ok(target_dir.join(filename))
    }

    fn resolve_directory(&self, path: &str) -> Result<PathBuf> {
        let absolute = self.to_absolute(path);
        let (canonical_ancestor, missing) = self.verified_ancestor(&absolute)?;

        let mut target = canonical_ancestor;

        for component in &missing {
            target.push(component);
        }

        if missing.is_empty() {
            if !target.is_dir() {
                bail!("Not a directory: {}", target.display());
            }

            Ok(target)
        } else {
            self.ensure_directory(target)
        }
    }

    fn ensure_inside_workspace(&self, path: &Path) -> Result<()> {
        if !path.starts_with(self.workspace.as_ref()) {
            bail!(
                "Path '{}' is outside the configured workspace '{}'",
                path.display(),
                self.workspace.display()
            );
        }

        Ok(())
    }
}

//
// -----------------------------------------------------------------------------
// MCP Request / Response structures
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ImageInfoRequest {
    /// Path to the image relative to the MCP workspace.
    input: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ResizeRequest {
    /// Input image path.
    input: String,

    /// Output image path.
    output: String,

    /// Maximum output width.
    width: Option<u32>,

    /// Maximum output height.
    height: Option<u32>,

    /// Preserve original aspect ratio.
    #[serde(default = "default_true")]
    preserve_aspect_ratio: bool,

    /// JPEG quality from 1 to 100. Ignored for PNG/WebP output, which is
    /// always encoded losslessly.
    #[serde(default = "default_quality")]
    quality: u8,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CompressRequest {
    /// Input image path.
    input: String,

    /// Output image path.
    output: String,

    /// Compression quality from 1 to 100. Applies to JPEG encoding only;
    /// PNG and WebP output is always lossless and ignores this value.
    #[serde(default = "default_quality")]
    quality: u8,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConvertRequest {
    /// Input image path.
    input: String,

    /// Output image path. Format is determined from the extension.
    output: String,

    /// JPEG quality from 1 to 100. Applies to JPEG encoding only; PNG and
    /// WebP output is always lossless and ignores this value.
    #[serde(default = "default_quality")]
    quality: u8,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OptimizeRequest {
    /// Input image path.
    input: String,

    /// Output image path.
    output: String,

    /// Maximum width. Image is not enlarged.
    max_width: Option<u32>,

    /// Maximum height. Image is not enlarged.
    max_height: Option<u32>,

    /// Target maximum file size in KB. Only JPEG output supports
    /// target-size optimization; for PNG/WebP output this is ignored and
    /// the response's `size_target_applied` field is set to `false`.
    max_size_kb: Option<u64>,

    /// JPEG quality floor. Ignored for PNG/WebP output.
    #[serde(default = "default_min_quality")]
    min_quality: u8,

    /// JPEG quality ceiling. Ignored for PNG/WebP output.
    #[serde(default = "default_quality")]
    max_quality: u8,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BatchOptimizeRequest {
    /// Directory containing images.
    directory: String,

    /// Output directory. If omitted, images are written to an "optimized"
    /// subdirectory of `directory`.
    output_directory: Option<String>,

    /// Maximum width.
    max_width: Option<u32>,

    /// Maximum height.
    max_height: Option<u32>,

    /// Maximum output size per image in KB. Only applies to JPEG files;
    /// see `size_target_applied` on each processed result.
    max_size_kb: Option<u64>,

    /// JPEG quality floor. Ignored for PNG/WebP output.
    #[serde(default = "default_min_quality")]
    min_quality: u8,

    /// JPEG quality ceiling. Ignored for PNG/WebP output.
    #[serde(default = "default_quality")]
    max_quality: u8,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct ImageInfo {
    path: String,
    format: String,
    width: u32,
    height: u32,
    size_bytes: u64,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
struct OperationResult {
    input: String,
    output: String,

    original_width: u32,
    original_height: u32,
    output_width: u32,
    output_height: u32,

    original_size_bytes: u64,
    output_size_bytes: u64,

    size_reduction_percent: f64,
    format: String,

    /// `None` when no target size was requested. `Some(true)` when
    /// target-size optimization ran (JPEG output). `Some(false)` when a
    /// target size was requested but ignored because the output format
    /// doesn't support it.
    size_target_applied: Option<bool>,
}

//
// -----------------------------------------------------------------------------
// Defaults
// -----------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

fn default_quality() -> u8 {
    82
}

fn default_min_quality() -> u8 {
    30
}

//
// -----------------------------------------------------------------------------
// Image helpers
// -----------------------------------------------------------------------------

fn load_image(path: &Path) -> Result<DynamicImage> {
    let image = ImageReader::open(path)
        .with_context(|| format!("Unable to open image: {}", path.display()))?
        .with_guessed_format()
        .context("Unable to determine image format")?
        .decode()
        .with_context(|| format!("Unable to decode image: {}", path.display()))?;

    Ok(image)
}

fn extension(path: &Path) -> Result<String> {
    path.extension()
        .and_then(|x| x.to_str())
        .map(|x| x.to_ascii_lowercase())
        .ok_or_else(|| anyhow::anyhow!("Output file has no supported extension"))
}

fn format_name(path: &Path) -> Result<&'static str> {
    match extension(path)?.as_str() {
        "jpg" | "jpeg" => Ok("jpeg"),
        "png" => Ok("png"),
        "webp" => Ok("webp"),
        other => bail!(
            "Unsupported image format '{}'. Supported formats: jpg, jpeg, png, webp",
            other
        ),
    }
}

fn calculate_dimensions(
    original_width: u32,
    original_height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> (u32, u32) {
    let mut width = original_width;
    let mut height = original_height;

    if let Some(max_width) = max_width
        && width > max_width
    {
        let ratio = max_width as f64 / width as f64;
        width = max_width;
        height = ((height as f64 * ratio).round() as u32).max(1);
    }

    if let Some(max_height) = max_height
        && height > max_height
    {
        let ratio = max_height as f64 / height as f64;
        height = max_height;
        width = ((width as f64 * ratio).round() as u32).max(1);
    }

    (width, height)
}

fn resize_if_needed(
    image: DynamicImage,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> DynamicImage {
    let (width, height) = image.dimensions();

    let (new_width, new_height) =
        calculate_dimensions(width, height, max_width, max_height);

    if width == new_width && height == new_height {
        image
    } else {
        image.resize_exact(
            new_width,
            new_height,
            FilterType::Lanczos3,
        )
    }
}

/// Creates (or truncates) the file at `path` for writing, refusing to
/// follow a symlink planted at that exact path. This closes the race
/// between a tool validating an output path (`resolve_output`) and the
/// later write: even if a symlink appears at the validated path in
/// between, the write will fail rather than follow it outside the
/// workspace.
#[cfg(unix)]
fn create_output_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("Unable to create {}", path.display()))
}

#[cfg(not(unix))]
fn create_output_file(path: &Path) -> Result<fs::File> {
    fs::File::create(path)
        .with_context(|| format!("Unable to create {}", path.display()))
}

fn encode_image(
    image: &DynamicImage,
    output: &Path,
    quality: u8,
) -> Result<()> {
    let file = create_output_file(output)?;

    let writer = BufWriter::new(file);

    let format = format_name(output)?;

    match format {
        "jpeg" => {
            let rgb = image.to_rgb8();

            let encoder = JpegEncoder::new_with_quality(
                writer,
                quality.clamp(1, 100),
            );

            encoder
                .write_image(
                    &rgb,
                    rgb.width(),
                    rgb.height(),
                    ColorType::Rgb8.into(),
                )
                .context("Failed to encode JPEG")?;
        }

        "png" => {
            let rgba = image.to_rgba8();

            let encoder = PngEncoder::new(writer);

            encoder
                .write_image(
                    &rgba,
                    rgba.width(),
                    rgba.height(),
                    ColorType::Rgba8.into(),
                )
                .context("Failed to encode PNG")?;
        }

        "webp" => {
            let rgba = image.to_rgba8();

            let encoder = WebPEncoder::new_lossless(writer);

            encoder
                .write_image(
                    &rgba,
                    rgba.width(),
                    rgba.height(),
                    ColorType::Rgba8.into(),
                )
                .context("Failed to encode WebP")?;
        }

        _ => unreachable!(),
    }

    Ok(())
}

/// Encodes `image`, optionally binary-searching JPEG quality to hit
/// `target_bytes`. Returns whether target-size optimization actually ran:
/// `None` if no target was requested, `Some(true)` if it ran (JPEG only),
/// `Some(false)` if a target was requested but the format doesn't support
/// it (so the caller can see the constraint wasn't silently dropped).
fn apply_size_optimization(
    image: &DynamicImage,
    output: &Path,
    format: &str,
    quality: u8,
    target_bytes: Option<u64>,
    min_quality: u8,
    max_quality: u8,
) -> Result<Option<bool>> {
    match target_bytes {
        Some(target) if format == "jpeg" => {
            optimize_jpeg_to_size(image, output, target, min_quality, max_quality)?;
            Ok(Some(true))
        }

        Some(_) => {
            encode_image(image, output, quality)?;
            Ok(Some(false))
        }

        None => {
            encode_image(image, output, quality)?;
            Ok(None)
        }
    }
}

fn calculate_reduction(original: u64, output: u64) -> f64 {
    if original == 0 {
        return 0.0;
    }

    ((original as f64 - output as f64) / original as f64) * 100.0
}

#[allow(clippy::too_many_arguments)]
fn operation_result(
    input: &Path,
    output: &Path,
    original_width: u32,
    original_height: u32,
    output_width: u32,
    output_height: u32,
    original_size: u64,
    size_target_applied: Option<bool>,
) -> Result<OperationResult> {
    let output_size = fs::metadata(output)
        .with_context(|| format!("Unable to read {}", output.display()))?
        .len();

    Ok(OperationResult {
        input: input.display().to_string(),
        output: output.display().to_string(),

        original_width,
        original_height,

        output_width,
        output_height,

        original_size_bytes: original_size,
        output_size_bytes: output_size,

        size_reduction_percent: calculate_reduction(
            original_size,
            output_size,
        ),

        format: format_name(output)?.to_string(),

        size_target_applied,
    })
}

//
// -----------------------------------------------------------------------------
// MCP tools
// -----------------------------------------------------------------------------

#[tool_router]
impl ImageServer {
    #[tool(
        description = "Inspect an image and return its dimensions, format and file size."
    )]
    async fn image_info(
        &self,
        Parameters(request): Parameters<ImageInfoRequest>,
    ) -> Result<String, String> {
        let input = self
            .resolve_input(&request.input)
            .map_err(|e| e.to_string())?;

        let image = load_image(&input)
            .map_err(|e| e.to_string())?;

        let (width, height) = image.dimensions();

        let size = fs::metadata(&input)
            .map_err(|e| e.to_string())?
            .len();

        let format = format!("{:?}", image.color());

        let extension = input
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("unknown")
            .to_string();

        let result = ImageInfo {
            path: input.display().to_string(),
            format: format!("{} ({})", extension, format),
            width,
            height,
            size_bytes: size,
        };

        serde_json::to_string_pretty(&result)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Resize an image. When preserve_aspect_ratio is true (default), the image is scaled to fit within width/height and never enlarged. When false, the image is resized to the exact width and height given, which may enlarge it."
    )]
    async fn resize_image(
        &self,
        Parameters(request): Parameters<ResizeRequest>,
    ) -> Result<String, String> {
        if request.width.is_none() && request.height.is_none() {
            return Err(
                "At least one of width or height must be specified."
                    .to_string(),
            );
        }

        validate_dimension("width", request.width)?;
        validate_dimension("height", request.height)?;
        validate_quality(request.quality)?;

        let input = self
            .resolve_input(&request.input)
            .map_err(|e| e.to_string())?;

        let output = self
            .resolve_output(&request.output)
            .map_err(|e| e.to_string())?;

        let image = load_image(&input)
            .map_err(|e| e.to_string())?;

        let original_size = fs::metadata(&input)
            .map_err(|e| e.to_string())?
            .len();

        let (original_width, original_height) = image.dimensions();

        let resized = if request.preserve_aspect_ratio {
            resize_if_needed(
                image,
                request.width,
                request.height,
            )
        } else {
            let width = request.width.unwrap_or(original_width);
            let height = request.height.unwrap_or(original_height);

            image.resize_exact(
                width,
                height,
                FilterType::Lanczos3,
            )
        };

        let (output_width, output_height) = resized.dimensions();

        encode_image(
            &resized,
            &output,
            request.quality,
        )
        .map_err(|e| e.to_string())?;

        let result = operation_result(
            &input,
            &output,
            original_width,
            original_height,
            output_width,
            output_height,
            original_size,
            None,
        )
        .map_err(|e| e.to_string())?;

        serde_json::to_string_pretty(&result)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Compress an image without changing its dimensions. Quality applies to JPEG encoding only; PNG and WebP output is always lossless."
    )]
    async fn compress_image(
        &self,
        Parameters(request): Parameters<CompressRequest>,
    ) -> Result<String, String> {
        validate_quality(request.quality)?;

        let input = self
            .resolve_input(&request.input)
            .map_err(|e| e.to_string())?;

        let output = self
            .resolve_output(&request.output)
            .map_err(|e| e.to_string())?;

        let image = load_image(&input)
            .map_err(|e| e.to_string())?;

        let original_size = fs::metadata(&input)
            .map_err(|e| e.to_string())?
            .len();

        let (width, height) = image.dimensions();

        encode_image(
            &image,
            &output,
            request.quality,
        )
        .map_err(|e| e.to_string())?;

        let result = operation_result(
            &input,
            &output,
            width,
            height,
            width,
            height,
            original_size,
            None,
        )
        .map_err(|e| e.to_string())?;

        serde_json::to_string_pretty(&result)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Convert an image between JPEG, PNG and WebP. Output format is determined from the output filename extension."
    )]
    async fn convert_image(
        &self,
        Parameters(request): Parameters<ConvertRequest>,
    ) -> Result<String, String> {
        validate_quality(request.quality)?;

        let input = self
            .resolve_input(&request.input)
            .map_err(|e| e.to_string())?;

        let output = self
            .resolve_output(&request.output)
            .map_err(|e| e.to_string())?;

        let image = load_image(&input)
            .map_err(|e| e.to_string())?;

        let original_size = fs::metadata(&input)
            .map_err(|e| e.to_string())?
            .len();

        let (width, height) = image.dimensions();

        encode_image(
            &image,
            &output,
            request.quality,
        )
        .map_err(|e| e.to_string())?;

        let result = operation_result(
            &input,
            &output,
            width,
            height,
            width,
            height,
            original_size,
            None,
        )
        .map_err(|e| e.to_string())?;

        serde_json::to_string_pretty(&result)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Optimize an image by optionally resizing it and compressing it toward a target file size. JPEG uses binary-search quality optimization; PNG/WebP ignore max_size_kb."
    )]
    async fn optimize_image(
        &self,
        Parameters(request): Parameters<OptimizeRequest>,
    ) -> Result<String, String> {
        validate_quality_range(request.min_quality, request.max_quality)?;
        validate_dimension("max_width", request.max_width)?;
        validate_dimension("max_height", request.max_height)?;

        let input = self
            .resolve_input(&request.input)
            .map_err(|e| e.to_string())?;

        let output = self
            .resolve_output(&request.output)
            .map_err(|e| e.to_string())?;

        let image = load_image(&input)
            .map_err(|e| e.to_string())?;

        let original_size = fs::metadata(&input)
            .map_err(|e| e.to_string())?
            .len();

        let (original_width, original_height) = image.dimensions();

        let image = resize_if_needed(
            image,
            request.max_width,
            request.max_height,
        );

        let (output_width, output_height) = image.dimensions();

        let target_bytes = compute_target_bytes(request.max_size_kb)?;

        let format = format_name(&output)
            .map_err(|e| e.to_string())?;

        let size_target_applied = apply_size_optimization(
            &image,
            &output,
            format,
            request.max_quality,
            target_bytes,
            request.min_quality,
            request.max_quality,
        )
        .map_err(|e| e.to_string())?;

        let result = operation_result(
            &input,
            &output,
            original_width,
            original_height,
            output_width,
            output_height,
            original_size,
            size_target_applied,
        )
        .map_err(|e| e.to_string())?;

        serde_json::to_string_pretty(&result)
            .map_err(|e| e.to_string())
    }

    #[tool(
        description = "Optimize every supported image in a directory. Supported formats: JPEG, PNG and WebP."
    )]
    async fn batch_optimize(
        &self,
        Parameters(request): Parameters<BatchOptimizeRequest>,
    ) -> Result<String, String> {
        validate_quality_range(request.min_quality, request.max_quality)?;
        validate_dimension("max_width", request.max_width)?;
        validate_dimension("max_height", request.max_height)?;

        let target_bytes = compute_target_bytes(request.max_size_kb)?;

        let input_dir = self
            .resolve_input_directory(&request.directory)
            .map_err(|e| e.to_string())?;

        let output_dir = match request.output_directory {
            Some(path) => self
                .resolve_directory(&path)
                .map_err(|e| e.to_string())?,
            None => self
                .ensure_directory(input_dir.join("optimized"))
                .map_err(|e| e.to_string())?,
        };

        let mut processed = Vec::new();
        let mut skipped = Vec::new();
        let mut failed = Vec::new();

        let entries = fs::read_dir(&input_dir)
            .map_err(|e| e.to_string())?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    failed.push(e.to_string());
                    continue;
                }
            };

            let path = entry.path();

            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(e) => {
                    failed.push(format!("{}: {}", path.display(), e));
                    continue;
                }
            };

            if file_type.is_symlink() {
                skipped.push(format!(
                    "{}: symlinks are not processed",
                    path.display()
                ));
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            let ext = path
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();

            let format = match ext.as_str() {
                "jpg" | "jpeg" => "jpeg",
                "png" => "png",
                "webp" => "webp",
                _ => {
                    skipped.push(path.display().to_string());
                    continue;
                }
            };

            let filename = match path.file_name() {
                Some(name) => name,
                None => continue,
            };

            let output = output_dir.join(filename);

            let image = match load_image(&path) {
                Ok(image) => image,
                Err(e) => {
                    failed.push(format!(
                        "{}: {}",
                        path.display(),
                        e
                    ));
                    continue;
                }
            };

            let original_size =
                match fs::metadata(&path) {
                    Ok(metadata) => metadata.len(),
                    Err(e) => {
                        failed.push(format!(
                            "{}: {}",
                            path.display(),
                            e
                        ));
                        continue;
                    }
                };

            let (original_width, original_height) =
                image.dimensions();

            let image = resize_if_needed(
                image,
                request.max_width,
                request.max_height,
            );

            let (output_width, output_height) =
                image.dimensions();

            let size_target_applied = match apply_size_optimization(
                &image,
                &output,
                format,
                request.max_quality,
                target_bytes,
                request.min_quality,
                request.max_quality,
            ) {
                Ok(applied) => applied,
                Err(e) => {
                    failed.push(format!(
                        "{}: {}",
                        path.display(),
                        e
                    ));
                    continue;
                }
            };

            match operation_result(
                &path,
                &output,
                original_width,
                original_height,
                output_width,
                output_height,
                original_size,
                size_target_applied,
            ) {
                Ok(result) => processed.push(result),
                Err(e) => failed.push(format!(
                    "{}: {}",
                    path.display(),
                    e
                )),
            }
        }

        let response = json!({
            "input_directory": input_dir.display().to_string(),
            "output_directory": output_dir.display().to_string(),
            "processed": processed,
            "processed_count": processed.len(),
            "skipped": skipped,
            "skipped_count": skipped.len(),
            "failed": failed,
            "failed_count": failed.len()
        });

        serde_json::to_string_pretty(&response)
            .map_err(|e| e.to_string())
    }
}

#[tool_handler]
impl ServerHandler for ImageServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(format!(
                "Image processing tools restricted to the workspace directory: {}",
                self.workspace.display()
            )),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

//
// -----------------------------------------------------------------------------
// JPEG target-size optimization
// -----------------------------------------------------------------------------

fn optimize_jpeg_to_size(
    image: &DynamicImage,
    output: &Path,
    target_bytes: u64,
    min_quality: u8,
    max_quality: u8,
) -> Result<()> {
    let rgb = image.to_rgb8();

    let mut low = min_quality;
    let mut high = max_quality;

    let mut best_quality = None;

    while low <= high {
        let quality = low + (high - low) / 2;

        encode_jpeg(
            &rgb,
            output,
            quality,
        )?;

        let size = fs::metadata(output)?.len();

        if size <= target_bytes {
            best_quality = Some(quality);

            if quality == 100 {
                break;
            }

            low = quality.saturating_add(1);
        } else {
            if quality == 0 {
                break;
            }

            high = quality.saturating_sub(1);
        }
    }

    if let Some(quality) = best_quality {
        encode_jpeg(
            &rgb,
            output,
            quality,
        )?;
    } else {
        // Even minimum quality could not meet target.
        // Leave the minimum-quality image as the best possible result.
        encode_jpeg(
            &rgb,
            output,
            min_quality,
        )?;
    }

    Ok(())
}

fn encode_jpeg(
    image: &image::RgbImage,
    output: &Path,
    quality: u8,
) -> Result<()> {
    let file = create_output_file(output)?;
    let writer = BufWriter::new(file);

    let encoder =
        JpegEncoder::new_with_quality(writer, quality);

    encoder.write_image(
        image,
        image.width(),
        image.height(),
        ColorType::Rgb8.into(),
    )?;

    Ok(())
}

//
// -----------------------------------------------------------------------------
// Validation
// -----------------------------------------------------------------------------

fn validate_quality(quality: u8) -> Result<(), String> {
    if !(1..=100).contains(&quality) {
        return Err(
            "quality must be between 1 and 100".to_string()
        );
    }

    Ok(())
}

fn validate_quality_range(min_quality: u8, max_quality: u8) -> Result<(), String> {
    validate_quality(min_quality)?;
    validate_quality(max_quality)?;

    if min_quality > max_quality {
        return Err(
            "min_quality cannot be greater than max_quality"
                .to_string(),
        );
    }

    Ok(())
}

fn validate_dimension(name: &str, value: Option<u32>) -> Result<(), String> {
    if let Some(0) = value {
        return Err(format!("{name} must be greater than 0"));
    }

    Ok(())
}

fn compute_target_bytes(max_size_kb: Option<u64>) -> Result<Option<u64>, String> {
    match max_size_kb {
        None => Ok(None),
        Some(kb) => kb
            .checked_mul(1024)
            .map(Some)
            .ok_or_else(|| "max_size_kb is too large".to_string()),
    }
}

//
// -----------------------------------------------------------------------------
// Main
// -----------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let workspace = parse_workspace(&args)?;

    let server = ImageServer::new(workspace)?;

    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .context("Failed to start MCP server")?;

    service
        .waiting()
        .await
        .context("MCP server stopped unexpectedly")?;

    Ok(())
}

fn parse_workspace(args: &[String]) -> Result<PathBuf> {
    let mut workspace = None;

    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--workspace" | "-w" => {
                if i + 1 >= args.len() {
                    bail!("--workspace requires a directory");
                }

                workspace = Some(PathBuf::from(&args[i + 1]));

                i += 2;
            }

            "--help" | "-h" => {
                println!(
                    "image-mcp\n\n\
                     Usage:\n\
                     image-mcp --workspace <directory>\n\n\
                     Options:\n\
                     -w, --workspace <directory>  Allowed filesystem workspace\n\
                     -h, --help                   Show this help"
                );

                std::process::exit(0);
            }

            unknown => {
                bail!("Unknown argument: {}", unknown);
            }
        }
    }

    workspace.ok_or_else(|| {
        anyhow::anyhow!(
            "--workspace is required.\n\
             Example: image-mcp --workspace /path/to/project"
        )
    })
}

//
// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn server_in(workspace: &Path) -> ImageServer {
        ImageServer::new(workspace.to_path_buf()).unwrap()
    }

    fn tiny_image() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 4, image::Rgb([200, 100, 50])))
    }

    // -- tool wiring ---------------------------------------------------

    #[test]
    fn server_exposes_all_documented_tools() {
        let workspace = TempDir::new().unwrap();
        let server = server_in(workspace.path());

        let names: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();

        for expected in [
            "image_info",
            "resize_image",
            "compress_image",
            "convert_image",
            "optimize_image",
            "batch_optimize",
        ] {
            assert!(names.contains(&expected.to_string()), "missing tool: {expected}");
        }
    }

    // -- path resolution / sandbox ---------------------------------------------------

    #[test]
    fn resolve_output_rejects_traversal_without_creating_directories() {
        let workspace = TempDir::new().unwrap();
        let outside_parent = workspace.path().parent().unwrap().to_path_buf();
        let escape_marker = outside_parent.join("image_mcp_test_escape_dir");
        let _ = fs::remove_dir_all(&escape_marker);

        let server = server_in(workspace.path());

        let result = server.resolve_output(&format!(
            "../{}/evil.png",
            escape_marker.file_name().unwrap().to_str().unwrap()
        ));

        assert!(result.is_err());
        assert!(!escape_marker.exists(), "traversal created a directory outside the workspace");
    }

    #[test]
    fn resolve_directory_rejects_traversal_without_creating_directories() {
        let workspace = TempDir::new().unwrap();
        let outside_parent = workspace.path().parent().unwrap().to_path_buf();
        let escape_marker = outside_parent.join("image_mcp_test_escape_batch_dir");
        let _ = fs::remove_dir_all(&escape_marker);

        let server = server_in(workspace.path());

        let result = server.resolve_directory(&format!(
            "../{}",
            escape_marker.file_name().unwrap().to_str().unwrap()
        ));

        assert!(result.is_err());
        assert!(!escape_marker.exists(), "traversal created a directory outside the workspace");
    }

    #[test]
    fn resolve_output_allows_new_nested_file_inside_workspace() {
        let workspace = TempDir::new().unwrap();
        let server = server_in(workspace.path());

        let output = server.resolve_output("nested/dir/out.png").unwrap();

        assert!(output.starts_with(fs::canonicalize(workspace.path()).unwrap()));
        assert!(output.parent().unwrap().is_dir());
        assert!(!output.exists());
    }

    #[test]
    #[cfg(unix)]
    fn resolve_output_rejects_symlink_escaping_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let target = outside.path().join("secret.png");
        fs::write(&target, b"secret").unwrap();

        let link = workspace.path().join("evil.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let server = server_in(workspace.path());
        let result = server.resolve_output("evil.png");

        assert!(result.is_err());
    }

    #[test]
    fn resolve_input_rejects_workspace_root_as_input() {
        let workspace = TempDir::new().unwrap();
        let server = server_in(workspace.path());

        let result = server.resolve_input(".");

        assert!(result.is_err());
    }

    #[test]
    fn resolve_input_directory_rejects_traversal() {
        let workspace = TempDir::new().unwrap();
        let server = server_in(workspace.path());

        let result = server.resolve_input_directory("..");

        assert!(result.is_err());
    }

    #[test]
    #[cfg(unix)]
    fn create_output_file_rejects_symlink_planted_after_resolution() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();

        let server = server_in(workspace.path());
        let output = server.resolve_output("out.png").unwrap();

        let target = outside.path().join("leaked.png");
        fs::write(&target, b"secret").unwrap();
        std::os::unix::fs::symlink(&target, &output).unwrap();

        let result = create_output_file(&output);

        assert!(result.is_err(), "write followed a symlink planted after path resolution");
    }

    // -- validation helpers ---------------------------------------------------

    #[test]
    fn validate_dimension_rejects_zero() {
        assert!(validate_dimension("width", Some(0)).is_err());
        assert!(validate_dimension("width", Some(10)).is_ok());
        assert!(validate_dimension("width", None).is_ok());
    }

    #[test]
    fn validate_quality_range_rejects_inverted_bounds() {
        assert!(validate_quality_range(80, 20).is_err());
        assert!(validate_quality_range(200, 0).is_err());
        assert!(validate_quality_range(20, 80).is_ok());
    }

    #[test]
    fn compute_target_bytes_rejects_overflow() {
        assert!(compute_target_bytes(Some(u64::MAX)).is_err());
        assert_eq!(compute_target_bytes(Some(10)).unwrap(), Some(10 * 1024));
        assert_eq!(compute_target_bytes(None).unwrap(), None);
    }

    // -- size-target behavior ---------------------------------------------------

    #[test]
    fn apply_size_optimization_marks_non_jpeg_as_not_applied() {
        let workspace = TempDir::new().unwrap();
        let output = workspace.path().join("out.png");

        let applied = apply_size_optimization(
            &tiny_image(),
            &output,
            "png",
            80,
            Some(1024),
            30,
            80,
        )
        .unwrap();

        assert_eq!(applied, Some(false));
        assert!(output.exists());
    }

    #[test]
    fn apply_size_optimization_marks_jpeg_as_applied() {
        let workspace = TempDir::new().unwrap();
        let output = workspace.path().join("out.jpg");

        let applied = apply_size_optimization(
            &tiny_image(),
            &output,
            "jpeg",
            80,
            Some(1024 * 1024),
            30,
            80,
        )
        .unwrap();

        assert_eq!(applied, Some(true));
        assert!(output.exists());
    }

    #[test]
    fn apply_size_optimization_none_when_no_target_requested() {
        let workspace = TempDir::new().unwrap();
        let output = workspace.path().join("out.jpg");

        let applied = apply_size_optimization(
            &tiny_image(),
            &output,
            "jpeg",
            80,
            None,
            30,
            80,
        )
        .unwrap();

        assert_eq!(applied, None);
    }

    // -- batch_optimize integration ---------------------------------------------------

    fn write_test_jpeg(path: &Path) {
        encode_jpeg(&tiny_image().to_rgb8(), path, 80).unwrap();
    }

    #[tokio::test]
    async fn batch_optimize_default_output_is_subdirectory_of_input() {
        let workspace = TempDir::new().unwrap();
        let photos = workspace.path().join("photos");
        fs::create_dir_all(&photos).unwrap();
        write_test_jpeg(&photos.join("a.jpg"));

        let server = server_in(workspace.path());

        let response = server
            .batch_optimize(Parameters(BatchOptimizeRequest {
                directory: "photos".to_string(),
                output_directory: None,
                max_width: None,
                max_height: None,
                max_size_kb: None,
                min_quality: default_min_quality(),
                max_quality: default_quality(),
            }))
            .await
            .unwrap();

        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let output_directory = value["output_directory"].as_str().unwrap();
        let canonical_workspace = fs::canonicalize(workspace.path()).unwrap();

        assert!(PathBuf::from(output_directory).starts_with(&canonical_workspace));
        assert!(PathBuf::from(output_directory).starts_with(fs::canonicalize(&photos).unwrap()));
        assert_eq!(value["processed_count"], 1);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn batch_optimize_skips_symlinked_files() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let photos = workspace.path().join("photos");
        fs::create_dir_all(&photos).unwrap();

        let outside_image = outside.path().join("leak.jpg");
        write_test_jpeg(&outside_image);
        std::os::unix::fs::symlink(&outside_image, photos.join("link.jpg")).unwrap();

        let server = server_in(workspace.path());

        let response = server
            .batch_optimize(Parameters(BatchOptimizeRequest {
                directory: "photos".to_string(),
                output_directory: None,
                max_width: None,
                max_height: None,
                max_size_kb: None,
                min_quality: default_min_quality(),
                max_quality: default_quality(),
            }))
            .await
            .unwrap();

        let value: serde_json::Value = serde_json::from_str(&response).unwrap();

        assert_eq!(value["processed_count"], 0);
        assert_eq!(value["skipped_count"], 1);
    }

    #[tokio::test]
    async fn batch_optimize_rejects_invalid_quality_bounds() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("photos")).unwrap();

        let server = server_in(workspace.path());

        let result = server
            .batch_optimize(Parameters(BatchOptimizeRequest {
                directory: "photos".to_string(),
                output_directory: None,
                max_width: None,
                max_height: None,
                max_size_kb: None,
                min_quality: 90,
                max_quality: 10,
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn batch_optimize_rejects_zero_max_width() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("photos")).unwrap();

        let server = server_in(workspace.path());

        let result = server
            .batch_optimize(Parameters(BatchOptimizeRequest {
                directory: "photos".to_string(),
                output_directory: None,
                max_width: Some(0),
                max_height: None,
                max_size_kb: None,
                min_quality: default_min_quality(),
                max_quality: default_quality(),
            }))
            .await;

        assert!(result.is_err());
    }
}
