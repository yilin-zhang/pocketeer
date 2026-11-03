//! The shape every write operation in this crate shares.
//!
//! An operation is a `plan` function that returns a [`Plan`] (data,
//! serialisable, nothing done yet) and [`apply`], the one way to carry a plan
//! out. A plan remembers which card and library it was made against
//! ([`Pinned`]); `apply` checks they are still the ones mounted before the
//! plan touches anything. What a plan needs while it runs comes from
//! [`Resources`]. The CLI prints the plan, applies it unless `--dry-run`, then
//! prints the result; the TUI shows the plan in a dialog and applies it on
//! confirmation.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{
    Error, ErrorInfo, Pocket, Result,
    fsutil::FileId,
    library::{DB_FILE, Library},
    net::Net,
};

/// Progress events for long operations.
///
/// Core functions take a `Progress` callback and never print themselves.
/// The CLI turns these into stderr lines; the TUI into gauges.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A download is in flight.
    Download {
        url: String,
        received: u64,
        total: Option<u64>,
    },
    /// A short status line ("Extracting", "Applying 3 changes").
    Status(String),
    /// Discrete progress within the current status.
    Step { done: usize, total: usize },
}

pub type Progress<'a> = &'a mut dyn FnMut(Event);

/// Something that can be previewed and then applied with [`apply`].
pub trait Plan: Serialize {
    type Output: Applied;

    /// How many things applying this plan would change. Zero means there is
    /// nothing to do and the plan is not applied.
    fn changes(&self) -> usize;

    /// Problems found while planning that count as failures even though
    /// nothing ran (an unresolved save conflict, say). Default none.
    fn errors(&self) -> usize {
        0
    }

    /// The card and library this plan was made against.
    fn pinned(&self) -> &Pinned;

    /// Carries the plan out. Only [`apply`] can call it: it is the one that
    /// can make a [`Checked`].
    fn execute(
        &mut self,
        res: &mut Resources,
        progress: Progress<'_>,
        checked: Checked,
    ) -> Result<Self::Output>;
}

/// Proof that a plan's [`Pinned`] card and library are the ones in
/// `Resources`. Its field is private, so only [`apply`] makes one.
pub struct Checked(());

/// Applies `plan`: first makes sure the card and library it was made against
/// are still the ones mounted (else [`Error::Stale`]), then carries it out.
pub fn apply<P: Plan>(
    res: &mut Resources,
    plan: &mut P,
    progress: Progress<'_>,
) -> Result<P::Output> {
    plan.pinned().check(res)?;
    plan.execute(res, progress, Checked(()))
}

/// Which card and library a plan was made against: their folders and what
/// they were on disk then. A card swapped at the same mount point, or a
/// library replaced at the same path, reads differently.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pinned {
    card: Option<Pin>,
    library: Option<Pin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    root: PathBuf,
    id: FileId,
}

impl Pin {
    /// The card's root folder, still a Pocket card.
    fn card(pocket: &Pocket) -> Result<Self> {
        let root = Pocket::open(pocket.root())?.root().to_path_buf();
        Ok(Pin {
            id: FileId::of(&root)?,
            root,
        })
    }

    /// The library's folder, identified by its database file.
    fn library(lib: &Library) -> Result<Self> {
        Ok(Pin {
            root: lib.root().to_path_buf(),
            id: FileId::of(&lib.root().join(DB_FILE))?,
        })
    }
}

impl Pinned {
    pub(crate) fn card(pocket: &Pocket) -> Result<Self> {
        Ok(Pinned {
            card: Some(Pin::card(pocket)?),
            library: None,
        })
    }

    pub(crate) fn library(lib: &Library) -> Result<Self> {
        Ok(Pinned {
            card: None,
            library: Some(Pin::library(lib)?),
        })
    }

    pub(crate) fn and_card(mut self, pocket: &Pocket) -> Result<Self> {
        self.card = Some(Pin::card(pocket)?);
        Ok(self)
    }

    /// The library's folder, when the plan was made against one.
    pub(crate) fn library_root(&self) -> Option<&Path> {
        self.library.as_ref().map(|p| p.root.as_path())
    }

    /// Fails with [`Error::Stale`] unless `pocket` is the pinned card. For
    /// work that keeps state about a card between steps (the ROM index).
    pub(crate) fn check_card(&self, pocket: &Pocket) -> Result<()> {
        match &self.card {
            Some(pin) if Pin::card(pocket)? != *pin => Err(Error::Stale {
                what: "the card".into(),
            }),
            _ => Ok(()),
        }
    }

    /// Fails with [`Error::Stale`] unless `lib` is the pinned library.
    pub(crate) fn check_library(&self, lib: &Library) -> Result<()> {
        match &self.library {
            Some(pin) if Pin::library(lib)? != *pin => Err(Error::Stale {
                what: "the library".into(),
            }),
            _ => Ok(()),
        }
    }

    fn check(&self, res: &mut Resources) -> Result<()> {
        if self.card.is_some() {
            self.check_card(res.pocket()?)?;
        }
        if self.library.is_some() {
            self.check_library(res.library()?)?;
        }
        Ok(())
    }
}

/// What plans need while they run: the card, the library and the network,
/// each opened the first time a plan asks for it, so work that needs only one
/// of them never fails over the others. The front end says how to open each.
pub struct Resources {
    pocket: Lazy<Pocket>,
    library: Lazy<Library>,
    net: Lazy<Net>,
}

type Opener<T> = Box<dyn FnMut() -> Result<T> + Send>;

struct Lazy<T> {
    value: Option<T>,
    open: Opener<T>,
}

impl<T> Lazy<T> {
    fn new(open: impl FnMut() -> Result<T> + Send + 'static) -> Self {
        Lazy {
            value: None,
            open: Box::new(open),
        }
    }

    fn get(&mut self) -> Result<&mut T> {
        if self.value.is_none() {
            self.value = Some((self.open)()?);
        }
        Ok(self.value.as_mut().expect("opened above"))
    }
}

impl Resources {
    pub fn new(
        pocket: impl FnMut() -> Result<Pocket> + Send + 'static,
        library: impl FnMut() -> Result<Library> + Send + 'static,
        net: impl FnMut() -> Result<Net> + Send + 'static,
    ) -> Self {
        Resources {
            pocket: Lazy::new(pocket),
            library: Lazy::new(library),
            net: Lazy::new(net),
        }
    }

    pub fn pocket(&mut self) -> Result<&Pocket> {
        self.pocket.get().map(|p| &*p)
    }

    pub fn library(&mut self) -> Result<&mut Library> {
        self.library.get()
    }

    pub fn net(&mut self) -> Result<&Net> {
        self.net.get().map(|n| &*n)
    }
}

/// What applying a plan produced.
pub trait Applied: Serialize {
    /// Items that failed one by one while the rest went through.
    fn failures(&self) -> &[Failure] {
        &[]
    }

    fn failed(&self) -> usize {
        self.failures().len()
    }
}

/// One item that failed while the rest of a plan went through: which file,
/// and why.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Failure {
    pub path: PathBuf,
    pub error: ErrorInfo,
}

impl Failure {
    pub fn new(path: impl Into<PathBuf>, error: &crate::Error) -> Self {
        Failure {
            path: path.into(),
            error: error.to_info(),
        }
    }
}
