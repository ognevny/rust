//! This crate handles the user facing autodiff macro. For each `#[autodiff(...)]` attribute,
//! we create an [`AutoDiffItem`] which contains the source and target function names. The source
//! is the function to which the autodiff attribute is applied, and the target is the function
//! getting generated by us (with a name given by the user as the first autodiff arg).

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use crate::expand::{Decodable, Encodable, HashStable_Generic};
use crate::ptr::P;
use crate::{Ty, TyKind};

/// Forward and Reverse Mode are well known names for automatic differentiation implementations.
/// Enzyme does support both, but with different semantics, see DiffActivity. The First variants
/// are a hack to support higher order derivatives. We need to compute first order derivatives
/// before we compute second order derivatives, otherwise we would differentiate our placeholder
/// functions. The proper solution is to recognize and resolve this DAG of autodiff invocations,
/// as it's already done in the C++ and Julia frontend of Enzyme.
///
/// Documentation for using [reverse](https://enzyme.mit.edu/rust/rev.html) and
/// [forward](https://enzyme.mit.edu/rust/fwd.html) mode is available online.
#[derive(Clone, Copy, Eq, PartialEq, Encodable, Decodable, Debug, HashStable_Generic)]
pub enum DiffMode {
    /// No autodiff is applied (used during error handling).
    Error,
    /// The primal function which we will differentiate.
    Source,
    /// The target function, to be created using forward mode AD.
    Forward,
    /// The target function, to be created using reverse mode AD.
    Reverse,
}

/// Dual and Duplicated (and their Only variants) are getting lowered to the same Enzyme Activity.
/// However, under forward mode we overwrite the previous shadow value, while for reverse mode
/// we add to the previous shadow value. To not surprise users, we picked different names.
/// Dual numbers is also a quite well known name for forward mode AD types.
#[derive(Clone, Copy, Eq, PartialEq, Encodable, Decodable, Debug, HashStable_Generic)]
pub enum DiffActivity {
    /// Implicit or Explicit () return type, so a special case of Const.
    None,
    /// Don't compute derivatives with respect to this input/output.
    Const,
    /// Reverse Mode, Compute derivatives for this scalar input/output.
    Active,
    /// Reverse Mode, Compute derivatives for this scalar output, but don't compute
    /// the original return value.
    ActiveOnly,
    /// Forward Mode, Compute derivatives for this input/output and *overwrite* the shadow argument
    /// with it.
    Dual,
    /// Forward Mode, Compute derivatives for this input/output and *overwrite* the shadow argument
    /// with it. Drop the code which updates the original input/output for maximum performance.
    DualOnly,
    /// Reverse Mode, Compute derivatives for this &T or *T input and *add* it to the shadow argument.
    Duplicated,
    /// Reverse Mode, Compute derivatives for this &T or *T input and *add* it to the shadow argument.
    /// Drop the code which updates the original input for maximum performance.
    DuplicatedOnly,
    /// All Integers must be Const, but these are used to mark the integer which represents the
    /// length of a slice/vec. This is used for safety checks on slices.
    FakeActivitySize,
}
/// We generate one of these structs for each `#[autodiff(...)]` attribute.
#[derive(Clone, Eq, PartialEq, Encodable, Decodable, Debug, HashStable_Generic)]
pub struct AutoDiffItem {
    /// The name of the function getting differentiated
    pub source: String,
    /// The name of the function being generated
    pub target: String,
    pub attrs: AutoDiffAttrs,
}

#[derive(Clone, Eq, PartialEq, Encodable, Decodable, Debug, HashStable_Generic)]
pub struct AutoDiffAttrs {
    /// Conceptually either forward or reverse mode AD, as described in various autodiff papers and
    /// e.g. in the [JAX
    /// Documentation](https://jax.readthedocs.io/en/latest/_tutorials/advanced-autodiff.html#how-it-s-made-two-foundational-autodiff-functions).
    pub mode: DiffMode,
    /// A user-provided, batching width. If not given, we will default to 1 (no batching).
    /// Calling a differentiated, non-batched function through a loop 100 times is equivalent to:
    /// - Calling the function 50 times with a batch size of 2
    /// - Calling the function 25 times with a batch size of 4,
    /// etc. A batched function takes more (or longer) arguments, and might be able to benefit from
    /// cache locality, better re-usal of primal values, and other optimizations.
    /// We will (before LLVM's vectorizer runs) just generate most LLVM-IR instructions `width`
    /// times, so this massively increases code size. As such, values like 1024 are unlikely to
    /// work. We should consider limiting this to u8 or u16, but will leave it at u32 for
    /// experiments for now and focus on documenting the implications of a large width.
    pub width: u32,
    pub ret_activity: DiffActivity,
    pub input_activity: Vec<DiffActivity>,
}

impl AutoDiffAttrs {
    pub fn has_primal_ret(&self) -> bool {
        matches!(self.ret_activity, DiffActivity::Active | DiffActivity::Dual)
    }
}

impl DiffMode {
    pub fn is_rev(&self) -> bool {
        matches!(self, DiffMode::Reverse)
    }
    pub fn is_fwd(&self) -> bool {
        matches!(self, DiffMode::Forward)
    }
}

impl Display for DiffMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            DiffMode::Error => write!(f, "Error"),
            DiffMode::Source => write!(f, "Source"),
            DiffMode::Forward => write!(f, "Forward"),
            DiffMode::Reverse => write!(f, "Reverse"),
        }
    }
}

/// Active(Only) is valid in reverse-mode AD for scalar float returns (f16/f32/...).
/// Dual(Only) is valid in forward-mode AD for scalar float returns (f16/f32/...).
/// Const is valid for all cases and means that we don't compute derivatives wrt. this output.
/// That usually means we have a &mut or *mut T output and compute derivatives wrt. that arg,
/// but this is too complex to verify here. Also it's just a logic error if users get this wrong.
pub fn valid_ret_activity(mode: DiffMode, activity: DiffActivity) -> bool {
    if activity == DiffActivity::None {
        // Only valid if primal returns (), but we can't check that here.
        return true;
    }
    match mode {
        DiffMode::Error => false,
        DiffMode::Source => false,
        DiffMode::Forward => {
            activity == DiffActivity::Dual
                || activity == DiffActivity::DualOnly
                || activity == DiffActivity::Const
        }
        DiffMode::Reverse => {
            activity == DiffActivity::Const
                || activity == DiffActivity::Active
                || activity == DiffActivity::ActiveOnly
        }
    }
}

/// For indirections (ptr/ref) we can't use Active, since Active allocates a shadow value
/// for the given argument, but we generally can't know the size of such a type.
/// For scalar types (f16/f32/f64/f128) we can use Active and we can't use Duplicated,
/// since Duplicated expects a mutable ref/ptr and we would thus end up with a shadow value
/// who is an indirect type, which doesn't match the primal scalar type. We can't prevent
/// users here from marking scalars as Duplicated, due to type aliases.
pub fn valid_ty_for_activity(ty: &P<Ty>, activity: DiffActivity) -> bool {
    use DiffActivity::*;
    // It's always allowed to mark something as Const, since we won't compute derivatives wrt. it.
    if matches!(activity, Const) {
        return true;
    }
    if matches!(activity, Dual | DualOnly) {
        return true;
    }
    // FIXME(ZuseZ4) We should make this more robust to also
    // handle type aliases. Once that is done, we can be more restrictive here.
    if matches!(activity, Active | ActiveOnly) {
        return true;
    }
    matches!(ty.kind, TyKind::Ptr(_) | TyKind::Ref(..))
        && matches!(activity, Duplicated | DuplicatedOnly)
}
pub fn valid_input_activity(mode: DiffMode, activity: DiffActivity) -> bool {
    use DiffActivity::*;
    return match mode {
        DiffMode::Error => false,
        DiffMode::Source => false,
        DiffMode::Forward => {
            matches!(activity, Dual | DualOnly | Const)
        }
        DiffMode::Reverse => {
            matches!(activity, Active | ActiveOnly | Duplicated | DuplicatedOnly | Const)
        }
    };
}

impl Display for DiffActivity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DiffActivity::None => write!(f, "None"),
            DiffActivity::Const => write!(f, "Const"),
            DiffActivity::Active => write!(f, "Active"),
            DiffActivity::ActiveOnly => write!(f, "ActiveOnly"),
            DiffActivity::Dual => write!(f, "Dual"),
            DiffActivity::DualOnly => write!(f, "DualOnly"),
            DiffActivity::Duplicated => write!(f, "Duplicated"),
            DiffActivity::DuplicatedOnly => write!(f, "DuplicatedOnly"),
            DiffActivity::FakeActivitySize => write!(f, "FakeActivitySize"),
        }
    }
}

impl FromStr for DiffMode {
    type Err = ();

    fn from_str(s: &str) -> Result<DiffMode, ()> {
        match s {
            "Error" => Ok(DiffMode::Error),
            "Source" => Ok(DiffMode::Source),
            "Forward" => Ok(DiffMode::Forward),
            "Reverse" => Ok(DiffMode::Reverse),
            _ => Err(()),
        }
    }
}
impl FromStr for DiffActivity {
    type Err = ();

    fn from_str(s: &str) -> Result<DiffActivity, ()> {
        match s {
            "None" => Ok(DiffActivity::None),
            "Active" => Ok(DiffActivity::Active),
            "ActiveOnly" => Ok(DiffActivity::ActiveOnly),
            "Const" => Ok(DiffActivity::Const),
            "Dual" => Ok(DiffActivity::Dual),
            "DualOnly" => Ok(DiffActivity::DualOnly),
            "Duplicated" => Ok(DiffActivity::Duplicated),
            "DuplicatedOnly" => Ok(DiffActivity::DuplicatedOnly),
            _ => Err(()),
        }
    }
}

impl AutoDiffAttrs {
    pub fn has_ret_activity(&self) -> bool {
        self.ret_activity != DiffActivity::None
    }
    pub fn has_active_only_ret(&self) -> bool {
        self.ret_activity == DiffActivity::ActiveOnly
    }

    pub const fn error() -> Self {
        AutoDiffAttrs {
            mode: DiffMode::Error,
            width: 0,
            ret_activity: DiffActivity::None,
            input_activity: Vec::new(),
        }
    }
    pub fn source() -> Self {
        AutoDiffAttrs {
            mode: DiffMode::Source,
            width: 0,
            ret_activity: DiffActivity::None,
            input_activity: Vec::new(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.mode != DiffMode::Error
    }

    pub fn is_source(&self) -> bool {
        self.mode == DiffMode::Source
    }
    pub fn apply_autodiff(&self) -> bool {
        !matches!(self.mode, DiffMode::Error | DiffMode::Source)
    }

    pub fn into_item(self, source: String, target: String) -> AutoDiffItem {
        AutoDiffItem { source, target, attrs: self }
    }
}

impl fmt::Display for AutoDiffItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Differentiating {} -> {}", self.source, self.target)?;
        write!(f, " with attributes: {:?}", self.attrs)
    }
}
