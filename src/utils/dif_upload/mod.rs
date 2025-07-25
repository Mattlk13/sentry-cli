#![expect(clippy::unwrap_used, reason = "contains legacy code which uses unwrap")]

//! Searches, processes and uploads debug information files (DIFs). See
//! `DifUpload` for more information.

mod error;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto as _;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read as _, Seek as _, Write as _};
use std::iter::IntoIterator;
use std::mem::transmute;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::str;
use std::time::Duration;

use anyhow::{bail, format_err, Error, Result};
use console::style;
use log::{debug, info, warn};
use sha1_smol::Digest;
use symbolic::common::{Arch, AsSelf, ByteView, DebugId, SelfCell, Uuid};
use symbolic::debuginfo::macho::{BcSymbolMap, UuidMapping};
use symbolic::debuginfo::pe::PeObject;
use symbolic::debuginfo::sourcebundle::{SourceBundleWriter, SourceFileDescriptor};
use symbolic::debuginfo::{Archive, FileEntry, FileFormat, Object};
use symbolic::il2cpp::ObjectLineMapping;
use walkdir::WalkDir;
use which::which;
use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use self::error::ValidationError;
use crate::api::{Api, ChunkServerOptions, ChunkUploadCapability};
use crate::config::Config;
use crate::constants::{DEFAULT_MAX_DIF_SIZE, DEFAULT_MAX_WAIT};
use crate::utils::chunks;
use crate::utils::chunks::{Assemblable, BatchedSliceExt as _, ChunkOptions, Chunked, ItemSize};
use crate::utils::dif::ObjectDifFeatures;
use crate::utils::fs::{get_sha1_checksum, TempDir, TempFile};
use crate::utils::progress::{ProgressBar, ProgressStyle};
use crate::utils::ui::{copy_with_progress, make_byte_progress_bar};

/// A debug info file on the server.
pub use crate::api::DebugInfoFile;

/// Fallback maximum number of chunks in a batch for the legacy upload.
static MAX_CHUNKS: u64 = 64;

/// A Debug Information File.
///
/// This is primarily used to store inside the [`DifMatch`] so does not contain any
/// information already present there.  You probably should look whether you can use
/// [`DifMatch`] instead of this instead.
enum ParsedDif<'a> {
    Object(Box<Object<'a>>),
    BcSymbolMap,
    UuidMap,
    Il2Cpp,
}

impl<'slf, 'data: 'slf> AsSelf<'slf> for ParsedDif<'data> {
    type Ref = ParsedDif<'data>;

    fn as_self(&'slf self) -> &'slf Self::Ref {
        self
    }
}

/// Contains backing data for a `DifMatch`.
///
/// This can be used to store the actual data that a `FatObject` might be
/// relying upon, such as temporary files or extracted archives. It will be
/// disposed along with a `DifMatch` once it is dropped.
#[derive(Debug)]
enum DifBacking {
    Temp,
}

/// A handle to a debug information file found by `DifUpload`.
///
/// It contains a `FatObject` giving access to the metadata and contents of the
/// debug information file. `DifMatch::attachments` may contain supplemental
/// files used to further process this file, such as dSYM PLists.
struct DifMatch<'data> {
    _backing: Option<DifBacking>,
    dif: SelfCell<ByteView<'data>, ParsedDif<'data>>,
    name: String,
    debug_id: Option<DebugId>,
    attachments: Option<BTreeMap<String, ByteView<'static>>>,
}

impl<'data> DifMatch<'data> {
    fn from_temp_object<S>(temp_file: TempFile, name: S, debug_id: Option<DebugId>) -> Result<Self>
    where
        S: Into<String>,
    {
        let buffer = ByteView::open(temp_file.path()).map_err(Error::new)?;
        let dif = SelfCell::try_new(buffer, |b| {
            Object::parse(unsafe { &*b }).map(|object| ParsedDif::Object(Box::new(object)))
        })?;

        Ok(DifMatch {
            _backing: Some(DifBacking::Temp),
            dif,
            name: name.into(),
            debug_id,
            attachments: None,
        })
    }

    fn from_temp_line_mapping<S>(
        temp_file: TempFile,
        name: S,
        debug_id: Option<DebugId>,
    ) -> Result<Self>
    where
        S: Into<String>,
    {
        let buffer = ByteView::open(temp_file.path()).map_err(Error::new)?;
        let dif = SelfCell::try_new(buffer, |_| Ok::<_, anyhow::Error>(ParsedDif::Il2Cpp))?;

        Ok(DifMatch {
            _backing: Some(DifBacking::Temp),
            dif,
            name: name.into(),
            debug_id,
            attachments: None,
        })
    }
    /// Creates a [`DifMatch`] from a `.bcsymbolmap` file.
    ///
    /// The `uuid` is the DebugID of the symbolmap while `name` is the filename of the file.
    /// Normally the filename should be the `uuid` with `.bcsymbolmap` appended to it.
    fn from_bcsymbolmap(uuid: DebugId, name: String, data: ByteView<'static>) -> Result<Self> {
        let dif = SelfCell::try_new(data, |buf| {
            BcSymbolMap::parse(unsafe { &*buf }).map(|_| ParsedDif::BcSymbolMap)
        })?;

        Ok(Self {
            _backing: None,
            dif,
            name,
            debug_id: Some(uuid),
            attachments: None,
        })
    }

    fn from_plist(uuid: DebugId, name: String, data: ByteView<'static>) -> Result<Self> {
        let dif = SelfCell::try_new(data, |buf| {
            UuidMapping::parse_plist(uuid, unsafe { &*buf }).map(|_| ParsedDif::UuidMap)
        })?;

        Ok(Self {
            _backing: None,
            dif,
            name,
            debug_id: Some(uuid),
            attachments: None,
        })
    }

    /// Moves the specified temporary debug file to a safe location and assumes
    /// ownership. The file will be deleted in the file system when this
    /// `DifMatch` is dropped.
    ///
    /// The path must point to a `FatObject` containing exactly one `Object`.
    fn take_temp<P, S>(path: P, name: S) -> Result<Self>
    where
        P: AsRef<Path>,
        S: Into<String>,
    {
        // Even though we could supply the debug_id here from the object we do not, the
        // server will do the same anyway and we actually have control over the version of
        // the code running there so can fix bugs more reliably.
        let temp_file = TempFile::take(path)?;
        Self::from_temp_object(temp_file, name, None)
    }

    /// Returns the parsed [`Object`] of this DIF.
    pub fn object(&self) -> Option<&Object<'data>> {
        match self.dif.get() {
            ParsedDif::Object(ref obj) => Some(obj),
            ParsedDif::BcSymbolMap => None,
            ParsedDif::UuidMap => None,
            ParsedDif::Il2Cpp => None,
        }
    }

    pub fn format(&self) -> DifFormat {
        match self.dif.get() {
            ParsedDif::Object(ref object) => DifFormat::Object(object.file_format()),
            ParsedDif::BcSymbolMap => DifFormat::BcSymbolMap,
            ParsedDif::UuidMap => DifFormat::PList,
            ParsedDif::Il2Cpp => DifFormat::Il2Cpp,
        }
    }

    /// Returns the raw binary data of this DIF.
    pub fn data(&self) -> &[u8] {
        match self.dif.get() {
            ParsedDif::Object(ref obj) => obj.data(),
            ParsedDif::BcSymbolMap => self.dif.owner(),
            ParsedDif::UuidMap => self.dif.owner(),
            ParsedDif::Il2Cpp => self.dif.owner(),
        }
    }

    /// Returns the size of of this DIF in bytes.
    pub fn size(&self) -> u64 {
        self.data().len() as u64
    }

    /// Returns the path of this DIF relative to the search origin.
    pub fn path(&self) -> &str {
        &self.name
    }

    /// Returns the name of this DIF, including its file extension.
    pub fn file_name(&self) -> &str {
        Path::new(self.path())
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("Generic")
    }

    /// Returns attachments of this DIF, if any.
    pub fn attachments(&self) -> Option<&BTreeMap<String, ByteView<'static>>> {
        self.attachments.as_ref()
    }

    /// Determines whether this file needs resolution of hidden symbols.
    pub fn needs_symbol_map(&self) -> bool {
        // XCode release archives and dSYM bundles downloaded from iTunes
        // Connect contain Swift library symbols. These have caused various
        // issues in the past, so we ignore them for now. In particular, there
        // are never any BCSymbolMaps generated for them and the DBGOriginalUUID
        // in the plist is the UUID of the original dsym file.
        //
        // We *might* have to locate the original library in the Xcode
        // distribution, then build a new non-fat dSYM file from it and patch
        // the UUID.
        if self.file_name().starts_with("libswift") {
            return false;
        }

        match self.object() {
            Some(Object::MachO(ref macho)) => macho.requires_symbolmap(),
            _ => false,
        }
    }
}

impl fmt::Debug for DifMatch<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DifMatch")
            .field("name", &self.name)
            .field("format", &self.format())
            .field("debug_id", &self.debug_id)
            .field("object", &self.object())
            .finish()
    }
}

impl AsRef<[u8]> for DifMatch<'_> {
    fn as_ref(&self) -> &[u8] {
        self.data()
    }
}

impl Display for DifMatch<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let kind = match self.dif.get() {
            ParsedDif::Object(ref object) => match object.kind() {
                symbolic::debuginfo::ObjectKind::None => String::new(),
                k => format!(" {k:#}"),
            },
            ParsedDif::BcSymbolMap => String::from("bcsymbolmap"),
            ParsedDif::UuidMap => String::from("uuidmap"),
            ParsedDif::Il2Cpp => String::from("il2cpp"),
        };

        write!(
            f,
            "{} ({}; {}{kind})",
            style(self.debug_id.map(|id| id.to_string()).unwrap_or_default()).dim(),
            self.name,
            self.object()
                .map(|object| {
                    let arch = object.arch();
                    match arch {
                        Arch::Unknown => String::new(),
                        _ => arch.to_string(),
                    }
                })
                .unwrap_or_default(),
        )
    }
}

impl Assemblable for DifMatch<'_> {
    /// A DIF's name is its file name.
    fn name(&self) -> Cow<str> {
        self.file_name().into()
    }

    fn debug_id(&self) -> Option<DebugId> {
        self.debug_id
    }
}

/// A `DifMatch` with computed SHA1 checksum.
#[derive(Debug)]
struct HashedDifMatch<'data> {
    inner: DifMatch<'data>,
    checksum: Digest,
}

impl<'data> HashedDifMatch<'data> {
    /// Calculates the SHA1 checksum for the given DIF.
    fn from(inner: DifMatch<'data>) -> Result<Self> {
        let checksum = get_sha1_checksum(inner.data())?;
        Ok(HashedDifMatch { inner, checksum })
    }

    /// Returns the SHA1 checksum of this DIF.
    fn checksum(&self) -> Digest {
        self.checksum
    }
}

impl<'data> Deref for HashedDifMatch<'data> {
    type Target = DifMatch<'data>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ItemSize for HashedDifMatch<'_> {
    fn size(&self) -> u64 {
        self.deref().size()
    }
}

type ZipFileArchive = ZipArchive<BufReader<File>>;

/// A handle to the source of a potential `DifMatch` used inside `search_difs`.
///
/// The primary use of this handle is to resolve files relative to the debug
/// information file and store them in `DifMatch::attachments`. These could be
/// companion files or metadata files needed to process the DIFs in sentry-cli,
/// or later even on Sentry.
#[derive(Debug)]
enum DifSource<'a> {
    /// A file located in the file system
    FileSystem(&'a Path),
    /// An entry in a ZIP file
    Zip(&'a mut ZipFileArchive, &'a str),
}

impl DifSource<'_> {
    /// Resolves a file relative to the directory of `base`, stripping of the
    /// file name.
    fn get_relative_fs(base: &Path, path: &Path) -> Option<ByteView<'static>> {
        // Use parent() to get to the directory and then move relative from
        // there. ByteView will internally canonicalize the path and resolve
        // symlinks.
        base.parent()
            .and_then(|p| ByteView::open(p.join(path)).ok())
    }

    /// Extracts a file relative to the directory of `name`, stripping of the
    /// file name.
    fn get_relative_zip(
        zip: &mut ZipFileArchive,
        name: &str,
        path: &Path,
    ) -> Option<ByteView<'static>> {
        // There is no built-in utility that normalizes paths without access to
        // the file system. We start by removing the file name from the given
        // path and then start to manually resolve the path components to a
        // final path.
        let mut zip_path = PathBuf::from(name);
        zip_path.pop();

        for component in path.components() {
            match component {
                Component::ParentDir => {
                    zip_path.pop();
                }
                Component::Normal(p) => {
                    zip_path.push(p);
                }
                _ => {
                    // `Component::CurDir` leaves the path as-is, and the
                    // remaining `Component::RootDir` and `Component::Prefix` do
                    // not make sense in ZIP files.
                }
            }
        }

        zip_path
            .to_str()
            .and_then(|name| zip.by_name(name).ok())
            .and_then(|f| ByteView::read(f).ok())
    }

    /// Resolves a file relative to this source and reads it into a `ByteView`.
    ///
    /// The target is always resolved relative to the directory of the source,
    /// excluding its file name. The path "../changed" relative to a source
    /// pointing to "path/to/file" will resolve in "path/changed".
    ///
    /// The returned ByteView will allow random-access to the data until it is
    /// disposed. If the source points to a ZIP file, the target is fully read
    /// into a memory buffer. See `ByteView::from_reader` for more information.
    pub fn get_relative<P>(&mut self, path: P) -> Option<ByteView<'static>>
    where
        P: AsRef<Path>,
    {
        match *self {
            DifSource::FileSystem(base) => Self::get_relative_fs(base, path.as_ref()),
            DifSource::Zip(ref mut zip, name) => Self::get_relative_zip(zip, name, path.as_ref()),
        }
    }
}

/// Verifies that the given path contains a ZIP file and opens it.
fn try_open_zip<P>(path: P) -> Result<Option<ZipFileArchive>>
where
    P: AsRef<Path>,
{
    if path.as_ref().extension() != Some("zip".as_ref()) {
        return Ok(None);
    }

    let mut magic: [u8; 2] = [0; 2];
    let mut file = File::open(path)?;
    if file.read_exact(&mut magic).is_err() {
        // Catch empty or single-character files
        return Ok(None);
    }

    file.rewind()?;
    Ok(match &magic {
        b"PK" => Some(ZipArchive::new(BufReader::new(file))?),
        _ => None,
    })
}

/// Searches the given ZIP for potential DIFs and passes them to the callback.
///
/// To avoid unnecessary file operations, the file extension is already checked
/// for every entry before opening it.
///
/// This function will not recurse into ZIPs contained in this ZIP.
fn walk_difs_zip<F>(mut zip: ZipFileArchive, options: &DifUpload, mut func: F) -> Result<()>
where
    F: FnMut(DifSource<'_>, String, ByteView<'static>) -> Result<()>,
{
    for index in 0..zip.len() {
        let (name, buffer) = {
            let mut zip_file = zip.by_index(index)?;
            let name = zip_file.name().to_owned();

            if !options.valid_extension(Path::new(&name).extension()) {
                continue;
            }

            let tmp_file = TempFile::create()?;
            let mut tmp_fh = tmp_file.open()?;
            std::io::copy(&mut zip_file, &mut tmp_fh)?;

            (name, ByteView::map_file(tmp_fh).map_err(Error::new)?)
        };

        func(DifSource::Zip(&mut zip, &name), name.clone(), buffer)?;
    }

    Ok(())
}

/// Recursively searches the given location for potential DIFs and passes them
/// to the callback.
///
/// If `DifUpload::allow_zips` is set, then this function will attempt to open
/// the ZIP and search it for DIFs as well, however not recursing further into
/// nested ZIPs.
///
/// To avoid unnecessary file operations, the file extension is already checked
/// for every entry before opening it.
fn walk_difs_directory<F, P>(location: P, options: &DifUpload, mut func: F) -> Result<()>
where
    P: AsRef<Path>,
    F: FnMut(DifSource<'_>, String, ByteView<'static>) -> Result<()>,
{
    let location = location.as_ref();
    let directory = if location.is_dir() {
        location
    } else {
        location.parent().unwrap_or_else(|| Path::new(""))
    };

    debug!("searching location {}", location.display());
    for entry in WalkDir::new(location)
        .follow_links(true)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.metadata()?.is_file() {
            // Walkdir recurses automatically into folders
            continue;
        }

        let path = entry.path();
        if options.zips_allowed {
            match try_open_zip(path) {
                Ok(Some(zip)) => {
                    debug!("searching zip archive {}", path.display());
                    if let Err(err) = walk_difs_zip(zip, options, &mut func) {
                        if let Some(e) = err.downcast_ref::<ZipError>() {
                            debug!("skipping zip archive {}", path.display());
                            debug!("error: {}", e);
                            continue;
                        };
                        return Err(err);
                    }
                    debug!("finished zip archive {}", path.display());
                    continue;
                }
                Err(e) => {
                    debug!("skipping zip archive {}", path.display());
                    debug!("error: {}", e);
                    continue;
                }
                Ok(None) => {
                    // this is not a zip archive
                }
            }
        }

        if !options.valid_extension(path.extension()) {
            continue;
        }

        let buffer = ByteView::open(path).map_err(Error::new)?;
        let name = path
            .strip_prefix(directory)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        func(DifSource::FileSystem(path), name, buffer)?;
    }

    debug!("finished location {}", directory.display());
    Ok(())
}

/// Searches for mapping PLists next to the given `source`. It returns a mapping
/// of Plist name to owning buffer of the file's contents. This function should
/// only be called for dSYMs.
fn find_uuid_plists(
    object: &Object<'_>,
    source: &mut DifSource<'_>,
) -> Option<BTreeMap<String, ByteView<'static>>> {
    let uuid = object.debug_id().uuid();
    if uuid.is_nil() {
        return None;
    }

    // When uploading an XCode build archive to iTunes Connect, Apple will
    // re-build the app for different architectures, causing new UUIDs in the
    // final bundle. To allow mapping back to the original symbols, it adds
    // PList files in the `Resources` folder (one level above the binary) that
    // contains the original UUID, one for each object contained in the fat
    // object.
    //
    // The folder structure looks like this:
    //
    //     App.dSYM
    //     ├─ Info.plist
    //     └─ Resources
    //        ├─ 1B205CD0-67D0-4D69-A0FA-C6BDDDB2A609.plist
    //        ├─ 1C228684-3EE5-472B-AB8D-29B3FBF63A70.plist
    //        └─ DWARF
    //           └─ App
    let plist_name = format!("{:X}.plist", uuid.as_hyphenated());
    let plist = source.get_relative(format!("../{}", &plist_name))?;

    let mut plists = BTreeMap::new();
    plists.insert(plist_name, plist);
    Some(plists)
}

/// Patch debug identifiers for PDBs where the corresponding PE specifies a different age.
fn fix_pdb_ages(difs: &mut [DifMatch<'_>], age_overrides: &BTreeMap<Uuid, u32>) {
    for dif in difs {
        if let Some(object) = dif.object() {
            if object.file_format() != FileFormat::Pdb {
                continue;
            }

            let debug_id = object.debug_id();
            let age = match age_overrides.get(&debug_id.uuid()) {
                Some(age) => *age,
                None => continue,
            };

            if age == debug_id.appendix() {
                continue;
            }

            debug!(
                "overriding age for {} ({} -> {})",
                dif.name,
                debug_id.appendix(),
                age
            );

            dif.debug_id = Some(DebugId::from_parts(debug_id.uuid(), age));
        }
    }
}

/// Searches matching debug information files.
fn search_difs(options: &DifUpload) -> Result<Vec<DifMatch<'static>>> {
    let progress_style = ProgressStyle::default_spinner().template(
        "{spinner} Searching for debug symbol files...\
         \n  found {prefix:.yellow} {msg:.dim}",
    );

    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(100);
    pb.set_style(progress_style);

    let mut age_overrides = BTreeMap::new();
    let mut collected = Vec::new();
    for base_path in &options.paths {
        if base_path == Path::new("") {
            warn!(
                "Skipping uploading from an empty path (\"\"). \
                Maybe you expanded an empty shell variable?"
            );
            continue;
        }
        walk_difs_directory(base_path, options, |source, name, buffer| {
            debug!("trying to process {}", name);
            pb.set_message(&name);

            if Archive::peek(&buffer) != FileFormat::Unknown {
                let mut difs =
                    collect_object_dif(source, name, buffer, options, &mut age_overrides)?;
                collected.append(difs.as_mut());
            } else if BcSymbolMap::test(&buffer) {
                if let Some(dif) = collect_auxdif(name, buffer, options, AuxDifKind::BcSymbolMap)? {
                    collected.push(dif);
                }
            } else if buffer.starts_with(b"<?xml") {
                if let Some(dif) = collect_auxdif(name, buffer, options, AuxDifKind::UuidMap)? {
                    collected.push(dif);
                }
            };

            pb.set_prefix(&collected.len().to_string());
            Ok(())
        })?;
    }

    if !age_overrides.is_empty() {
        fix_pdb_ages(&mut collected, &age_overrides);
    }

    pb.finish_and_clear();

    print!(
        "{} Found {} debug information {}",
        style(">").dim(),
        style(collected.len()).yellow(),
        match collected.len() {
            1 => "file",
            _ => "files",
        }
    );

    let count_with_sources = collected
        .iter()
        .filter(|dif| match dif.object() {
            Some(object) => object.has_sources(),
            None => false,
        })
        .count();

    match count_with_sources {
        0 => println!(),
        _ => println!(" ({count_with_sources} with embedded sources)"),
    }

    Ok(collected)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AuxDifKind {
    BcSymbolMap,
    UuidMap,
}

impl Display for AuxDifKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AuxDifKind::BcSymbolMap => write!(f, "BCSymbolMap"),
            AuxDifKind::UuidMap => write!(f, "UuidMap"),
        }
    }
}

/// Collects a possible BCSymbolmap or PList into a [`DifMatch`].
///
/// The `name` is the relative path of the file processed, while `buffer` contains the
/// actual data.
fn collect_auxdif<'a>(
    name: String,
    buffer: ByteView<'static>,
    options: &DifUpload,
    kind: AuxDifKind,
) -> Result<Option<DifMatch<'a>>> {
    let file_stem = Path::new(&name)
        .file_stem()
        .map(|stem| stem.to_string_lossy())
        .unwrap_or_default();
    let uuid: DebugId = match file_stem.parse() {
        Ok(uuid) => uuid,
        Err(_) => {
            if kind == AuxDifKind::BcSymbolMap {
                // There are loads of plists in a normal XCode Archive that are not valid
                // UUID mappings.  Warning for all these is pointless.
                warn!(
                    "Skipping {kind} with invalid filename: {name}",
                    kind = kind,
                    name = name
                );
            }
            return Ok(None);
        }
    };
    let dif_result = match kind {
        AuxDifKind::BcSymbolMap => DifMatch::from_bcsymbolmap(uuid, name.clone(), buffer),
        AuxDifKind::UuidMap => DifMatch::from_plist(uuid, name.clone(), buffer),
    };
    let dif = match dif_result {
        Ok(dif) => dif,
        Err(err) => {
            warn!(
                "Skipping invalid {kind} file {name}: {err}",
                kind = kind,
                name = name,
                err = err
            );
            return Ok(None);
        }
    };

    // Skip this file if we don't want to process it.
    if let Err(e) = options.validate_dif(&dif) {
        error::handle(&name, &e)?;
        return Ok(None);
    }

    Ok(Some(dif))
}

/// Processes and [`DifSource`] which is expected to be an object file.
fn collect_object_dif<'a>(
    mut source: DifSource<'_>,
    name: String,
    buffer: ByteView<'static>,
    options: &DifUpload,
    age_overrides: &mut BTreeMap<Uuid, u32>,
) -> Result<Vec<DifMatch<'a>>> {
    let mut collected = Vec::with_capacity(2);

    // Try to parse a potential object file. If this is not possible,
    // then we're not dealing with an object file, thus silently
    // skipping it.
    let format = Archive::peek(&buffer);

    // Override this behavior for PE files. Their debug identifier is
    // needed in case PDBs should be uploaded to fix an eventual age
    // mismatch
    let should_override_age =
        format == FileFormat::Pe && options.valid_format(DifFormat::Object(FileFormat::Pdb));

    if !should_override_age && !options.valid_format(DifFormat::Object(format)) {
        return Ok(collected);
    }

    debug!("trying to parse dif {}", name);
    let archive = match Archive::parse(&buffer) {
        Ok(archive) => archive,
        Err(e) => {
            warn!("Skipping invalid debug file {}: {}", name, e);
            return Ok(collected);
        }
    };

    // Each `FatObject` might contain multiple matching objects, each of
    // which needs to retain a reference to the original fat file. We
    // create a shared instance here and clone it into `DifMatch`es
    // below.
    for object in archive.objects() {
        // Skip all objects that we cannot process. This can
        // happen due to invalid object files, which we then just
        // discard rather than stopping the scan.
        let object = match object {
            Ok(object) => object,
            Err(e) => {
                warn!("Skipping invalid object: {e}");
                continue;
            }
        };

        // Objects without debug id will be skipped altogether. While frames
        // during symbolication might be lacking debug identifiers,
        // Sentry requires object files to have one during upload.
        let id = object.debug_id();
        if id.is_nil() {
            debug!("Skipping object without debug id");
            continue;
        }

        // If this is a PE file with an embedded Portable PDB, we extract and process the PPDB separately.
        if let Object::Pe(pe) = &object {
            if let Ok(Some(ppdb_dif)) = extract_embedded_ppdb(pe, name.as_str()) {
                if let Err(e) = options.validate_dif(&ppdb_dif) {
                    error::handle(&ppdb_dif.name, &e)?;
                } else {
                    collected.push(ppdb_dif);
                }
            }
        };

        // Store a mapping of "age" values for all encountered PE files,
        // regardless of whether they will be uploaded. This is used later
        // to fix up PDB files.
        if should_override_age {
            age_overrides.insert(id.uuid(), id.appendix());
        }

        // Invoke logic to retrieve attachments specific to the kind
        // of object file. These are used for processing. Since only
        // dSYMs require processing currently, all other kinds are
        // skipped.
        let attachments = match object.file_format() {
            FileFormat::MachO => find_uuid_plists(&object, &mut source),
            _ => None,
        };

        // We retain the buffer and the borrowed object in a new SelfCell. This is
        // incredibly unsafe, but in our case it is fine, since the SelfCell owns the same
        // buffer that was used to retrieve the object.
        #[expect(clippy::missing_transmute_annotations)]
        let cell = unsafe {
            SelfCell::from_raw(
                buffer.clone(),
                ParsedDif::Object(Box::new(transmute(object))),
            )
        };

        let dif = DifMatch {
            _backing: None,
            dif: cell,
            name: name.clone(),
            debug_id: Some(id),
            attachments,
        };

        // Skip this file if we don't want to process it.
        if let Err(e) = options.validate_dif(&dif) {
            error::handle(&name, &e)?;
            continue;
        }

        collected.push(dif);
    }

    Ok(collected)
}

/// Resolves BCSymbolMaps and replaces hidden symbols in a `DifMatch` using
/// `dsymutil`. If successful, this will return a new `DifMatch` based on a
/// temporary file. The original dSYM is not touched.
///
/// Note that this process copies the file to a temporary location and might
/// incur significant I/O for larger debug files.
fn resolve_hidden_symbols<'a>(dif: DifMatch<'a>, symbol_map: &Path) -> Result<DifMatch<'a>> {
    if dif.attachments.is_none() {
        println!(
            "{} {}: Could not locate UUID mapping for {}",
            style(">").dim(),
            style("Warning").red(),
            style(dif.file_name()).yellow(),
        );
        return Ok(dif);
    }

    // We need to rebuild the Resources folder of a dSYM structure in a temp
    // directory that is guaranteed to be deleted after this operation. The
    // Info.plist is not needed for this operation:
    //     Resources
    //     ├─ 1B205CD0-67D0-4D69-A0FA-C6BDDDB2A609.plist
    //     ├─ 1C228684-3EE5-472B-AB8D-29B3FBF63A70.plist
    //     └─ DWARF
    //        └─ ObjectFile
    let temp_dir = TempDir::create()?;
    fs::create_dir_all(temp_dir.path().join("DWARF"))?;

    // Copy the object file binary
    let temp_path = temp_dir.path().join("DWARF").join(dif.file_name());
    let mut temp_file = File::create(&temp_path)?;
    temp_file.write_all(dif.data())?;
    temp_file.sync_data()?;

    // Copy the UUID plists
    for (name, view) in dif.attachments().unwrap() {
        let mut plist = File::create(temp_dir.path().join(name))?;
        plist.write_all(view)?;
        plist.sync_data()?;
    }

    let output = Command::new("dsymutil")
        .arg("-symbol-map")
        .arg(symbol_map)
        .arg(&temp_path)
        .output()?;

    if !output.status.success() {
        if let Ok(error) = str::from_utf8(&output.stderr) {
            bail!("Could not resolve BCSymbolMaps: {}", error);
        } else {
            bail!("Could not resolve BCSymbolMaps due to an unknown error");
        }
    }

    // Take ownership of the modified (fat) object file and move it somewhere
    // else so it is safe to delete the temp directory.
    DifMatch::take_temp(temp_path, dif.path())
}

/// Runs all `DifMatch` objects through the provided callback and displays a
/// progress bar while doing so.
///
/// ```
/// prepare_difs(processed, |m| HashedDifMatch::from(m))?
/// ```
fn prepare_difs<'data, F, T>(items: Vec<DifMatch<'data>>, mut func: F) -> Result<Vec<T>>
where
    F: FnMut(DifMatch<'data>) -> Result<T>,
{
    let progress_style = ProgressStyle::default_bar().template(
        "{prefix:.dim} Preparing for upload... {msg:.dim}\
         \n{wide_bar}  {pos}/{len}",
    );

    let pb = ProgressBar::new(items.len());
    pb.set_style(progress_style);
    pb.set_prefix(">");

    let mut calculated = Vec::new();
    for item in items {
        pb.inc(1);
        pb.set_message(item.path());
        calculated.push(func(item)?);
    }

    pb.finish_and_clear();
    println!(
        "{} Prepared debug information {} for upload",
        style(">").dim(),
        match calculated.len() {
            1 => "file",
            _ => "files",
        }
    );

    Ok(calculated)
}

/// Resolves BCSymbolMaps for all debug files with hidden symbols. All other
/// files are not touched. Note that this only applies to Apple dSYMs.
///
/// If there are debug files with hidden symbols but no `symbol_map` path is
/// given, a warning is emitted.
fn process_symbol_maps<'a>(
    difs: Vec<DifMatch<'a>>,
    symbol_map: Option<&Path>,
) -> Result<Vec<DifMatch<'a>>> {
    let (with_hidden, mut without_hidden): (Vec<_>, _) =
        difs.into_iter().partition(DifMatch::needs_symbol_map);

    if with_hidden.is_empty() {
        return Ok(without_hidden);
    }

    let symbol_map = match symbol_map {
        Some(path) => path,
        _ => {
            println!(
                "{} {}: Found {} symbol files with hidden symbols (need BCSymbolMaps)",
                style(">").dim(),
                style("Warning").red(),
                style(with_hidden.len()).yellow()
            );

            without_hidden.extend(with_hidden);
            return Ok(without_hidden);
        }
    };

    let len = with_hidden.len();
    let progress_style = ProgressStyle::default_bar().template(
        "{prefix:.dim} Resolving BCSymbolMaps... {msg:.dim}\
         \n{wide_bar}  {pos}/{len}",
    );

    let pb = ProgressBar::new(len);
    pb.set_style(progress_style);
    pb.set_prefix(">");

    for dif in with_hidden {
        pb.inc(1);
        pb.set_message(dif.path());
        without_hidden.push(resolve_hidden_symbols(dif, symbol_map)?);
    }

    pb.finish_and_clear();
    println!(
        "{} Resolved BCSymbolMaps for {} debug information {}",
        style(">").dim(),
        style(len).yellow(),
        match len {
            1 => "file",
            _ => "files",
        }
    );

    Ok(without_hidden)
}

/// Checks whether the `PeObject` contains an embedded Portable PDB and extracts it as a separate  `DifMatch`.
fn extract_embedded_ppdb<'a>(pe: &PeObject, pe_name: &str) -> Result<Option<DifMatch<'a>>> {
    if let Some(embedded_ppdb) = pe.embedded_ppdb()? {
        let temp_file = TempFile::create()?;
        temp_file
            .open()
            .map(|f| embedded_ppdb.decompress_to(BufWriter::new(f)))??;

        let dif = DifMatch::from_temp_object(
            temp_file,
            Path::new(pe_name).with_extension("pdb").to_string_lossy(),
            Some(pe.debug_id()),
        )?;
        Ok(Some(dif))
    } else {
        Ok(None)
    }
}

/// Default filter function to skip over bad sources we do not want to include.
pub fn filter_bad_sources(
    entry: &FileEntry,
    embedded_source: &Option<SourceFileDescriptor>,
) -> bool {
    let max_size = Config::current().get_max_dif_item_size();
    let path = &entry.abs_path_str();

    // Ignore pch files.
    if path.ends_with(".pch") {
        return false;
    }

    // Ignore files embedded in the object itself.
    if embedded_source.is_some() {
        debug!("Skipping embedded source file: {}", path);
        return false;
    }

    // Ignore files larger than limit (defaults to `DEFAULT_MAX_DIF_ITEM_SIZE`).
    if let Ok(meta) = fs::metadata(path) {
        let item_size = meta.len();
        if item_size > max_size {
            warn!(
                "Source exceeded maximum item size limit ({}). {}",
                item_size, path
            );
            return false;
        }
    }

    // if a file metadata could not be read it will be skipped later.
    debug!("Trying to add source file: {}", path);
    true
}

/// Creates a source bundle containing the source files referenced by the input DIFs.
///
/// If `include_il2cpp_sources` is true, C# files referenced by il2cpp line mapping comments
/// will also be included.
fn create_source_bundles<'a>(
    difs: &[DifMatch<'a>],
    include_il2cpp_sources: bool,
) -> Result<Vec<DifMatch<'a>>> {
    let mut source_bundles = Vec::new();

    let progress_style = ProgressStyle::default_bar().template(
        "{prefix:.dim} Resolving source code... {msg:.dim}\
         \n{wide_bar}  {pos}/{len}",
    );

    let pb = ProgressBar::new(difs.len());
    pb.set_style(progress_style);
    pb.set_prefix(">");

    for dif in difs {
        let name = dif.path();
        pb.inc(1);
        pb.set_message(name);
        debug!("trying to collect sources for {}", name);

        let object = match dif.object() {
            Some(object) => object,
            None => continue,
        };

        let temp_file = TempFile::create()?;
        let mut writer = SourceBundleWriter::start(BufWriter::new(temp_file.open()?))?;
        writer.collect_il2cpp_sources(include_il2cpp_sources);

        // Resolve source files from the object and write their contents into the archive. Skip to
        // upload this bundle if no source could be written. This can happen if there is no file or
        // line information in the object file, or if none of the files could be resolved.
        let written = writer
            .with_skipped_file_callback(|skipped_info| info!("{skipped_info}"))
            .write_object_with_filter(object, dif.file_name(), filter_bad_sources)?;
        if !written {
            debug!("No sources found for {}", name);
            continue;
        }

        source_bundles.push(DifMatch::from_temp_object(temp_file, name, dif.debug_id)?);
    }

    let len = source_bundles.len();
    pb.finish_and_clear();
    println!(
        "{} Resolved source code for {} debug information {}",
        style(">").dim(),
        style(len).yellow(),
        match len {
            1 => "file",
            _ => "files",
        }
    );

    Ok(source_bundles)
}

fn create_il2cpp_mappings<'a>(difs: &[DifMatch<'a>]) -> Result<Vec<DifMatch<'a>>> {
    let mut line_mappings = Vec::new();

    let progress_style = ProgressStyle::default_bar().template(
        "{prefix:.dim} Resolving il2cpp mappings... {msg:.dim}\
         \n{wide_bar}  {pos}/{len}",
    );

    let pb = ProgressBar::new(difs.len());
    pb.set_style(progress_style);
    pb.set_prefix(">");

    for dif in difs {
        pb.inc(1);
        pb.set_message(dif.path());

        if let Some(object) = dif.object() {
            let temp_file = TempFile::create()?;
            let written = {
                let mut writer = BufWriter::new(temp_file.open()?);

                let line_mapping = ObjectLineMapping::from_object(object)?;
                let written = line_mapping.to_writer(&mut writer)?;
                writer.flush()?;
                written
            };
            if !written {
                continue;
            }

            let line_mapping =
                DifMatch::from_temp_line_mapping(temp_file, dif.path(), dif.debug_id)?;
            line_mappings.push(line_mapping);
        }
    }

    let len = line_mappings.len();
    pb.finish_and_clear();
    println!(
        "{} Resolved il2cpp mappings for {} debug information {}",
        style(">").dim(),
        style(len).yellow(),
        match len {
            1 => "file",
            _ => "files",
        }
    );

    Ok(line_mappings)
}

/// Uploads debug info files using the chunk-upload endpoint.
fn upload_difs_chunked(
    options: DifUpload,
    chunk_options: ChunkServerOptions,
) -> Result<(Vec<DebugInfoFile>, bool)> {
    // Search for debug files in the file system and ZIPs
    let found = search_difs(&options)?;
    if found.is_empty() {
        println!("{} No debug information files found", style(">").dim());
        return Ok(Default::default());
    }

    // Try to resolve BCSymbolMaps
    let symbol_map = options.symbol_map.as_deref();
    let mut processed = process_symbol_maps(found, symbol_map)?;

    if options.upload_il2cpp_mappings {
        let il2cpp_mappings = create_il2cpp_mappings(&processed)?;
        processed.extend(il2cpp_mappings);
    }

    // Resolve source code context if specified
    if options.include_sources {
        let source_bundles = create_source_bundles(&processed, options.upload_il2cpp_mappings)?;
        processed.extend(source_bundles);
    }

    // Calculate checksums and chunks
    let chunked = prepare_difs(processed, |m| {
        Chunked::from(m, chunk_options.chunk_size as usize)
    })?;

    let options = options.into_chunk_options(chunk_options);
    chunks::upload_chunked_objects(&chunked, options)
}

/// Returns debug files missing on the server.
fn get_missing_difs<'data>(
    objects: Vec<HashedDifMatch<'data>>,
    options: &DifUpload,
) -> Result<Vec<HashedDifMatch<'data>>> {
    info!(
        "Checking for missing debug information files: {:#?}",
        &objects
    );

    let api = Api::current();
    let missing_checksums = {
        let checksums = objects.iter().map(HashedDifMatch::checksum);
        api.authenticated()?
            .find_missing_dif_checksums(options.org, options.project, checksums)?
    };

    let missing = objects
        .into_iter()
        .filter(|sym| missing_checksums.contains(&sym.checksum()))
        .collect();

    info!("Missing debug information files: {:#?}", &missing);
    Ok(missing)
}

/// Compresses the given batch into a ZIP archive.
fn create_batch_archive(difs: &[HashedDifMatch<'_>]) -> Result<TempFile> {
    let total_bytes = difs.iter().map(ItemSize::size).sum();
    let pb = make_byte_progress_bar(total_bytes);
    let tf = TempFile::create()?;

    {
        let mut zip = ZipWriter::new(tf.open()?);

        for symbol in difs {
            zip.start_file(symbol.file_name(), SimpleFileOptions::default())?;
            copy_with_progress(&pb, &mut symbol.data(), &mut zip)?;
        }
    }

    pb.finish_and_clear();
    Ok(tf)
}

/// Uploads the given DIFs to the server in batched ZIP archives.
fn upload_in_batches(
    objects: &[HashedDifMatch<'_>],
    options: &DifUpload,
) -> Result<Vec<DebugInfoFile>> {
    let api = Api::current();
    let max_size = Config::current().get_max_dif_archive_size();
    let mut dsyms = Vec::new();

    for (i, (batch, _)) in objects.batches(max_size, MAX_CHUNKS).enumerate() {
        println!("\n{}", style(format!("Batch {}", i + 1)).bold());

        println!(
            "{} Compressing {} debug symbol files",
            style(">").dim(),
            style(batch.len()).yellow()
        );
        let archive = create_batch_archive(batch)?;

        println!("{} Uploading debug symbol files", style(">").dim());
        dsyms.extend(
            api.authenticated()?
                .region_specific(options.org)
                .upload_dif_archive(options.project, archive.path())?,
        );
    }

    Ok(dsyms)
}

/// Uploads debug info files using the legacy endpoint.
fn upload_difs_batched(options: &DifUpload) -> Result<Vec<DebugInfoFile>> {
    // Search for debug files in the file system and ZIPs
    let found = search_difs(options)?;
    if found.is_empty() {
        println!("{} No debug information files found", style(">").dim());
        return Ok(Default::default());
    }

    // Try to resolve BCSymbolMaps
    let symbol_map = options.symbol_map.as_deref();
    let processed = process_symbol_maps(found, symbol_map)?;

    // Calculate checksums
    let hashed = prepare_difs(processed, HashedDifMatch::from)?;

    // Check which files are missing on the server
    let missing = get_missing_difs(hashed, options)?;
    if missing.is_empty() {
        println!(
            "{} Nothing to upload, all files are on the server",
            style(">").dim()
        );
        println!("{} Nothing to upload", style(">").dim());
        return Ok(Default::default());
    }

    // Upload missing DIFs in batches
    let uploaded = upload_in_batches(&missing, options)?;
    if !uploaded.is_empty() {
        println!("{} File upload complete:\n", style(">").dim());
        for dif in &uploaded {
            println!(
                "  {} ({}; {})",
                style(&dif.id()).dim(),
                &dif.object_name,
                dif.cpu_name
            );
        }
    }

    Ok(uploaded)
}

/// The format of a Debug Information File (DIF).
///
/// Most DIFs are also object files, but we also know of some auxiliary DIF formats.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DifFormat {
    /// An object file of some kind, as per [`symbolic::debuginfo::FileFormat`].
    ///
    /// Not all these are fully objects, but they all implement
    /// [`symbolic::debuginfo::ObjectLike`].
    Object(FileFormat),
    /// An Apple BCSymbolMap.
    BcSymbolMap,
    /// An Apple PList.
    ///
    /// This only considers PLists which contain a
    /// [`symbolic::debuginfo::macho::UuidMapping`] used to map a `dSYM` UUID back to UUID
    /// of the original `BCSymbolMap`.
    PList,
    /// A Unity il2cpp line mapping file.
    Il2Cpp,
}

/// Searches, processes and uploads debug information files (DIFs).
///
/// This struct is created with the `DifUpload::new` function. Then, set
/// search parameters and start the upload via `DifUpload::upload`.
///
/// ```
/// use utils::dif_upload::DifUpload;
///
/// DifUpload::new("org".into(), "project".into())
///     .search_path(".")
///     .upload()?;
/// ```
///
/// The upload tries to perform a chunked upload by requesting the new
/// `chunk-upload/` endpoint. If chunk uploads are disabled or the server does
/// not support them yet, it falls back to the legacy `files/dsyms/` endpoint.
///
/// The uploader will walk the given `paths` in the file system recursively and
/// search for DIFs. If `allow_zips` is not deactivated, it will also open ZIP
/// files and search there.
///
/// By default, all supported object files will be included. To customize this,
/// use the `filter_id`, `filter_kind`, `filter_class` and `filter_extension`
/// methods.
///
/// If `symbol_map` is set and Apple dSYMs with hidden symbols are found, the
/// uploader will first try to locate BCSymbolMaps and generate new dSYMs with
/// resolved symbols.
#[derive(Debug, Default)]
pub struct DifUpload<'a> {
    org: &'a str,
    project: &'a str,
    paths: Vec<PathBuf>,
    ids: BTreeSet<DebugId>,
    formats: BTreeSet<DifFormat>,
    features: ObjectDifFeatures,
    extensions: BTreeSet<OsString>,
    symbol_map: Option<PathBuf>,
    zips_allowed: bool,
    max_file_size: u64,
    max_wait: Duration,
    pdbs_allowed: bool,
    portablepdbs_allowed: bool,
    sources_allowed: bool,
    include_sources: bool,
    bcsymbolmaps_allowed: bool,
    wait: bool,
    upload_il2cpp_mappings: bool,
    il2cpp_mappings_allowed: bool,
}

impl<'a> DifUpload<'a> {
    /// Creates a new `DifUpload` with default parameters.
    ///
    /// To use it, also add paths using `DifUpload::search_path`. It will scan
    /// the paths and contained ZIPs for all supported object files and upload
    /// them.
    ///
    /// Use `DifUpload::symbol_map` to configure a location of BCSymbolMap files
    /// to resolve hidden symbols in dSYMs obtained from iTunes Connect.
    ///
    /// ```
    /// use utils::dif_upload::DifUpload;
    ///
    /// DifUpload::new("org", "project")
    ///     .search_path(".")
    ///     .upload()?;
    /// ```
    pub fn new(org: &'a str, project: &'a str) -> Self {
        DifUpload {
            org,
            project,
            paths: Vec::new(),
            ids: BTreeSet::new(),
            formats: BTreeSet::new(),
            features: ObjectDifFeatures::all(),
            extensions: BTreeSet::new(),
            symbol_map: None,
            zips_allowed: true,
            max_file_size: DEFAULT_MAX_DIF_SIZE,
            max_wait: DEFAULT_MAX_WAIT,
            pdbs_allowed: false,
            portablepdbs_allowed: false,
            sources_allowed: false,
            include_sources: false,
            bcsymbolmaps_allowed: false,
            wait: false,
            upload_il2cpp_mappings: false,
            il2cpp_mappings_allowed: false,
        }
    }

    /// Adds a path to search for debug information files.
    pub fn search_path<P>(&mut self, path: P) -> &mut Self
    where
        P: Into<PathBuf>,
    {
        self.paths.push(path.into());
        self
    }

    /// Adds paths to search for debug information files.
    pub fn search_paths<I>(&mut self, paths: I) -> &mut Self
    where
        I: IntoIterator,
        I::Item: Into<PathBuf>,
    {
        for path in paths {
            self.paths.push(path.into())
        }
        self
    }

    /// Add `DebugId`s to filter for.
    ///
    /// By default, all DebugIds will be included. If `ids` is empty, this will
    /// not be changed.
    pub fn filter_ids<I>(&mut self, ids: I) -> &mut Self
    where
        I: IntoIterator,
        I::Item: Into<DebugId>,
    {
        for id in ids {
            self.ids.insert(id.into());
        }
        self
    }

    /// Add an `FileFormat` to filter for.
    ///
    /// By default, all object formats will be included.
    pub fn filter_format(&mut self, format: DifFormat) -> &mut Self {
        self.formats.insert(format);
        self
    }

    /// Add an `ObjectFeature` to filter for.
    ///
    /// By default, all object features will be included.
    pub fn filter_features(&mut self, features: ObjectDifFeatures) -> &mut Self {
        self.features = features;
        self
    }

    /// Set a path containing BCSymbolMaps to resolve hidden symbols in dSYMs
    /// obtained from iTunes Connect. This requires the `dsymutil` command.
    ///
    /// By default, hidden symbol resolution will be skipped.
    pub fn symbol_map<P>(&mut self, path: P) -> Result<&mut Self>
    where
        P: Into<PathBuf>,
    {
        which("dsymutil").map_err(|_| format_err!("Command `dsymutil` not found"))?;
        self.symbol_map = Some(path.into());
        Ok(self)
    }

    /// Set whether opening and searching ZIPs for debug information files is
    /// allowed or not.
    ///
    /// Defaults to `true`.
    pub fn allow_zips(&mut self, allow: bool) -> &mut Self {
        self.zips_allowed = allow;
        self
    }

    /// Set whether source files should be resolved during the scan process and
    /// uploaded as a separate archive.
    ///
    /// Defaults to `false`.
    pub fn include_sources(&mut self, include: bool) -> &mut Self {
        self.include_sources = include;
        self
    }

    /// Set whether the upload should wait for the server to complete processing
    /// files or exit immediately after the upload.
    ///
    /// Defaults to `false`.
    pub fn wait(&mut self, wait: bool) -> &mut Self {
        self.wait = wait;
        self
    }

    /// Sets the maximum length of time the upload should wait for the server to complete processing.
    ///
    /// Defaults to [`DEFAULT_MAX_WAIT`].
    pub fn max_wait(&mut self, max_wait: Duration) -> &mut Self {
        self.max_wait = max_wait;
        self
    }

    /// Set whether il2cpp line mappings should be computed and uploaded.
    ///
    /// Defaults to `false`.
    pub fn il2cpp_mapping(&mut self, il2cpp_mapping: bool) -> &mut Self {
        self.upload_il2cpp_mappings = il2cpp_mapping;
        self
    }

    /// Performs the search for DIFs and uploads them.
    ///
    /// ```
    /// use utils::dif_upload::DifUpload;
    ///
    /// DifUpload::new("org", "project")
    ///     .search_path(".")
    ///     .upload()?;
    /// ```
    ///
    /// The okay part of the return value is `(files, has_errors)`.  The
    /// latter can be used to indicate a fail state from the upload.
    pub fn upload(mut self) -> Result<(Vec<DebugInfoFile>, bool)> {
        if self.paths.is_empty() {
            println!("{}: No paths were provided.", style("Warning").yellow());
            return Ok(Default::default());
        }

        let api = Api::current();
        if let Some(chunk_options) = api.authenticated()?.get_chunk_upload_options(self.org)? {
            if chunk_options.max_file_size > 0 {
                self.max_file_size = chunk_options.max_file_size;
            }
            if chunk_options.max_wait > 0 {
                self.max_wait = self
                    .max_wait
                    .min(Duration::from_secs(chunk_options.max_wait));
            }

            self.pdbs_allowed = chunk_options.supports(ChunkUploadCapability::Pdbs);
            self.portablepdbs_allowed = chunk_options.supports(ChunkUploadCapability::PortablePdbs);
            self.sources_allowed = chunk_options.supports(ChunkUploadCapability::Sources);
            self.bcsymbolmaps_allowed = chunk_options.supports(ChunkUploadCapability::BcSymbolmap);
            self.il2cpp_mappings_allowed = chunk_options.supports(ChunkUploadCapability::Il2Cpp);

            if chunk_options.supports(ChunkUploadCapability::DebugFiles) {
                self.validate_capabilities();
                return upload_difs_chunked(self, chunk_options);
            }
        }

        self.validate_capabilities();
        Ok((upload_difs_batched(&self)?, false))
    }

    /// Validate that the server supports all requested capabilities.
    fn validate_capabilities(&mut self) {
        // Checks whether source bundles are *explicitly* requested on the command line.
        if (self
            .formats
            .contains(&DifFormat::Object(FileFormat::SourceBundle))
            || self.include_sources)
            && !self.sources_allowed
        {
            warn!("Source uploads are not supported by the configured Sentry server");
            self.include_sources = false;
        }

        // Checks whether PDBs or PEs were *explicitly* requested on the command line.
        if (self.formats.contains(&DifFormat::Object(FileFormat::Pdb))
            || self.formats.contains(&DifFormat::Object(FileFormat::Pe)))
            && !self.pdbs_allowed
        {
            warn!("PDBs and PEs are not supported by the configured Sentry server");
            // This is validated additionally in .valid_format()
        }

        // Checks whether Portable PDBs were *explicitly* requested on the command line.
        if self
            .formats
            .contains(&DifFormat::Object(FileFormat::PortablePdb))
            && !self.portablepdbs_allowed
        {
            warn!("Portable PDBs are not supported by the configured Sentry server");
            // This is validated additionally in .valid_format()
        }

        // Checks whether BCSymbolMaps and PLists are **explicitly** requested on the command line.
        if (self.formats.contains(&DifFormat::BcSymbolMap)
            || self.formats.contains(&DifFormat::PList))
            && !self.bcsymbolmaps_allowed
        {
            warn!("BCSymbolMaps are not supported by the configured Sentry server");
        }

        if self.upload_il2cpp_mappings && !self.il2cpp_mappings_allowed {
            warn!("il2cpp line mappings are not supported by the configured Sentry server");
        }
    }

    /// Determines if this `DebugId` matches the search criteria.
    fn valid_id(&self, id: DebugId) -> bool {
        self.ids.is_empty() || self.ids.contains(&id)
    }

    /// Determines if this file extension matches the search criteria.
    fn valid_extension(&self, ext: Option<&OsStr>) -> bool {
        self.extensions.is_empty() || ext.is_some_and(|e| self.extensions.contains(e))
    }

    /// Determines if this [`DifFormat`] matches the search criteria.
    fn valid_format(&self, format: DifFormat) -> bool {
        match format {
            DifFormat::Object(FileFormat::Unknown) => false,
            DifFormat::Object(FileFormat::Pdb) if !self.pdbs_allowed => false,
            DifFormat::Object(FileFormat::Pe) if !self.pdbs_allowed => false,
            DifFormat::Object(FileFormat::SourceBundle) if !self.sources_allowed => false,
            DifFormat::Object(FileFormat::PortablePdb) if !self.portablepdbs_allowed => false,
            DifFormat::BcSymbolMap | DifFormat::PList if !self.bcsymbolmaps_allowed => false,
            format => self.formats.is_empty() || self.formats.contains(&format),
        }
    }

    /// Determines if the given `Object` matches the features search criteria.
    ///
    /// If this is not an Object DIF then the object features filter does not apply so this
    /// always returns that it is valid.
    fn valid_features(&self, dif: &DifMatch) -> bool {
        let object = match dif.object() {
            Some(object) => object,
            None => return true,
        };
        self.features.symtab && object.has_symbols()
            || self.features.debug && object.has_debug_info()
            || self.features.unwind && object.has_unwind_info()
            || self.features.sources && object.has_sources()
    }

    /// Checks if a file is too large and logs skip message if so.
    fn valid_size(&self, size: usize) -> bool {
        let file_size: Result<u64, _> = size.try_into();

        match file_size {
            Ok(file_size) => file_size <= self.max_file_size,
            Err(_) => false, // Definitely too big
        }
    }

    /// Validates DIF on whether it should be processed.
    ///
    /// This takes all the filters configured in the [`DifUpload`] into account and returns
    /// whether a file should be skipped or not.  It also takes care of logging such a skip
    /// if required.
    fn validate_dif(&self, dif: &DifMatch) -> Result<(), ValidationError> {
        if !self.valid_format(dif.format()) {
            return Err(ValidationError::InvalidFormat);
        }

        if !self.valid_features(dif) {
            return Err(ValidationError::InvalidFeatures);
        }

        let id = dif.debug_id.unwrap_or_default();
        if id.is_nil() || !self.valid_id(id) {
            return Err(ValidationError::InvalidDebugId);
        }

        if !self.valid_size(dif.data().len()) {
            return Err(ValidationError::TooLarge {
                size: dif.data().len(),
                max_size: self.max_file_size,
            });
        }

        Ok(())
    }

    fn into_chunk_options(self, server_options: ChunkServerOptions) -> ChunkOptions<'a> {
        let options = ChunkOptions::new(server_options, self.org, self.project);

        // Only add wait time if self.wait is true. On DifUpload, max_wait may be
        // set even when self.wait is false; on ChunkOptions, the absence of a
        // specific max_wait means we should not wait, and there is no separate
        // flag for whether to wait.
        if self.wait {
            options.with_max_wait(self.max_wait)
        } else {
            options
        }
    }
}
