//! A relay condition block as one typed field in an Anchor 1.0 account.
//!
//! [`relay_spec`] is deliberately framework-free (bytemuck only), so it
//! cannot implement anchor's `IdlBuild` — and without that, `anchor idl
//! build` rejects any account struct carrying a block. This crate owns that
//! coupling and nothing else: [`RelayBlockHost`] derefs to the spec type
//! (hosts call `init`, `write_resolvers`, and the whole
//! [`relay_spec::ConditionBlock`] surface straight through) and describes
//! itself to the IDL as what it is on the wire — an opaque byte region of the
//! instantiation's exact size.
//!
//! ```ignore
//! use relay_anchor::RelayBlock;
//!
//! #[account(zero_copy)]
//! pub struct MyThing {
//!     pub relay: RelayBlock<NUM_CONDITIONS, 8>,
//!     // ... host fields ...
//! }
//! // once, at account creation:
//! my_thing.relay.init(relay_spec::block_offset!(MyThing, relay) as u32)?;
//! ```
//!
//! Nothing here interprets a single byte of the region: reads, writes, and
//! layout all stay in the spec crate, which is what keeps this wrapper
//! correct by construction.
//!
//! ## Versions
//!
//! The wrapper is generic over the spec version it hosts
//! ([`RelayBlockVersion`]), not hardcoded to v0. [`RelayBlock`] is an alias
//! for the v0 region, which is what hosts name today; a future
//! `RelayBlockV1` becomes a second alias over the same wrapper, and hosting
//! it requires no change here and no change to a host that is staying on v0.
//!
//! The intended v0 → v1 migration, in the order it happens:
//!
//! 1. The spec crate adds `RelayBlockV1<...>` and implements
//!    [`RelayBlockVersion`] for it. Nothing existing moves — v0 blocks keep
//!    reading as v0, because the header carries the version byte.
//! 2. This crate gains the one-line alias (`RelayBlockV1Field`). Both alias
//!    to the same [`RelayBlockHost`], so both get `Deref`, `Pod`, the
//!    condition surface, and an IDL type for free.
//! 3. A host that wants v1 flips its field's type and, for accounts already
//!    on chain, reallocs by the size delta and runs the spec's in-place
//!    conversion (`ConditionBlock::migrate`, plus
//!    `RelayBlockV0::grow_in_place` where only the capacities changed). Both
//!    are spec-side operations; the wrapper is not involved.
//! 4. The generated IDL names the new type ([`RelayBlockVersion::idl_name`]
//!    includes the version), so clients see a distinct type rather than a
//!    silently resized one. That is deliberate: a client decoding a v1
//!    region as v0 is exactly the failure this naming prevents.
//!
//! A host may carry a v0 field and a v1 field at once during a rollout —
//! they are different types with different IDL names, and the wrapper treats
//! both as opaque bytes.

use core::fmt::Debug;

use relay_spec::{
    bytemuck::{Pod, Zeroable},
    ConditionBlock, RelayBlockV0,
};

/// A relay block layout the wrapper can host.
///
/// Implemented by the spec crate for its own block types; a host never
/// implements this. The three items are everything the wrapper needs and
/// nothing about the contents — size for the IDL, the spec version byte, and
/// the type name a generated IDL should use.
pub trait RelayBlockVersion: Pod + Default + Debug + ConditionBlock {
    /// Wire version the region's header carries.
    const SPEC_VERSION: u8;
    /// Exact wire size of the region.
    const SIZE: usize;
    /// Name this region takes in a generated IDL. Includes the version, so a
    /// client can never decode one version's bytes as another's.
    fn idl_name() -> String;
}

impl<const C: usize, const R: usize> RelayBlockVersion for RelayBlockV0<C, R> {
    const SPEC_VERSION: u8 = relay_spec::SPEC_VERSION;
    const SIZE: usize = RelayBlockV0::<C, R>::SIZE;

    fn idl_name() -> String {
        // The v0 spelling predates versioned naming and is kept as-is: it is
        // already in published IDLs, and renaming it would churn every
        // client for no gain.
        format!("RelayBlock{C}x{R}")
    }
}

/// One field hosting everything relay needs: the spec header, the condition
/// slots, and the resolver account list region the conditions point at.
///
/// Hosts name the version alias ([`RelayBlock`]) rather than this type
/// directly; see the module docs for why it is generic.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RelayBlockHost<V: RelayBlockVersion>(pub V);

/// The v0 region: `CONDITIONS` condition slots and a `RESOLVER_CAPACITY`-slot
/// resolver account list. See [`relay_spec::RelayBlockV0`] for sizing
/// guidance — both parameters are capacities, and spare slots are cheaper
/// than ever having to grow one.
pub type RelayBlock<const CONDITIONS: usize, const RESOLVER_CAPACITY: usize> =
    RelayBlockHost<RelayBlockV0<CONDITIONS, RESOLVER_CAPACITY>>;

impl<V: RelayBlockVersion> core::ops::Deref for RelayBlockHost<V> {
    type Target = V;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<V: RelayBlockVersion> core::ops::DerefMut for RelayBlockHost<V> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// repr(transparent) over a Pod type, so the wrapper has that type's layout
// exactly — same size, same alignment, no padding of its own.
unsafe impl<V: RelayBlockVersion> Zeroable for RelayBlockHost<V> {}
unsafe impl<V: RelayBlockVersion> Pod for RelayBlockHost<V> {}

/// The condition surface, by delegation, so hosts can name the wrapper in
/// trait calls without deref gymnastics.
impl<V: RelayBlockVersion> ConditionBlock for RelayBlockHost<V> {
    const NUM_CONDITIONS: usize = V::NUM_CONDITIONS;

    fn block(&self) -> &[u8] {
        ConditionBlock::block(&self.0)
    }

    fn block_mut(&mut self) -> &mut [u8] {
        ConditionBlock::block_mut(&mut self.0)
    }
}

/// Describe the region to `anchor idl build` as the opaque byte array it is.
///
/// A host's own `idl-build` feature must forward to this crate's:
/// `idl-build = ["anchor-lang/idl-build", "relay-anchor/idl-build", ...]`.
/// Without that the impl is not compiled and the IDL build fails on the
/// account struct that hosts the field.
#[cfg(feature = "idl-build")]
impl<V: RelayBlockVersion> anchor_lang::idl::IdlBuild for RelayBlockHost<V> {
    fn create_type() -> Option<anchor_lang::idl::types::IdlTypeDef> {
        use anchor_lang::idl::types::*;
        Some(IdlTypeDef {
            name: Self::get_full_path(),
            docs: vec![format!(
                "relay condition block (spec v{}), {} conditions, as one opaque wire region",
                V::SPEC_VERSION,
                V::NUM_CONDITIONS
            )],
            serialization: IdlSerialization::BytemuckUnsafe,
            repr: Some(IdlRepr::C(IdlReprModifier {
                packed: false,
                align: None,
            })),
            generics: vec![],
            ty: IdlTypeDefTy::Struct {
                fields: Some(IdlDefinedFields::Named(vec![IdlField {
                    name: "bytes".into(),
                    docs: vec![],
                    ty: IdlType::Array(Box::new(IdlType::U8), IdlArrayLen::Value(V::SIZE)),
                }])),
            },
        })
    }

    fn insert_types(
        types: &mut std::collections::BTreeMap<String, anchor_lang::idl::types::IdlTypeDef>,
    ) {
        if let Some(ty) = Self::create_type() {
            types.insert(ty.name.clone(), ty);
        }
    }

    fn get_full_path() -> String {
        V::idl_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_spec::{AccountRefV0, ConditionV0, CrankSpecV0, ResolverListV0, SpecError, WakeView};

    type Block = RelayBlock<3, 8>;
    const BLOCK_OFFSET: u32 = 8 + 16;

    fn spec() -> CrankSpecV0 {
        CrankSpecV0 {
            resolver_program: [1; 32],
            resolver_disc: [2; 8],
            min_payment: 5,
        }
    }

    /// The wrapper is the spec type's layout, exactly — that is the whole
    /// contract with a `#[account(zero_copy)]` host.
    #[test]
    fn the_wrapper_has_the_hosted_layout() {
        assert_eq!(
            core::mem::size_of::<Block>(),
            core::mem::size_of::<RelayBlockV0<3, 8>>()
        );
        assert_eq!(core::mem::size_of::<Block>(), RelayBlockV0::<3, 8>::SIZE);
        assert_eq!(core::mem::align_of::<Block>(), 1);
        assert_eq!(core::mem::size_of::<Block>() % 8, 0);
    }

    /// Hosts reach the spec API through `Deref`, and the condition surface by
    /// delegation — and the bytes that come out are the bare spec type's,
    /// byte for byte.
    #[test]
    fn hosting_through_the_wrapper_writes_spec_bytes() {
        let mut wrapped = Block::zeroed();
        wrapped.init(BLOCK_OFFSET).unwrap();
        let refs = [
            AccountRefV0::writable([1; 32]),
            AccountRefV0::readonly([2; 32]),
        ];
        let list = wrapped.write_resolvers(&refs).unwrap();
        assert_eq!(wrapped.account_offset(), BLOCK_OFFSET);
        assert_eq!(wrapped.resolver_refs(), &refs[..]);
        ConditionBlock::write_condition(
            &mut wrapped,
            0,
            &ConditionV0::at_timestamp(42, spec(), list),
        )
        .unwrap();

        let mut bare = RelayBlockV0::<3, 8>::zeroed();
        bare.init(BLOCK_OFFSET).unwrap();
        let list = bare.write_resolvers(&refs).unwrap();
        bare.write_condition(0, &ConditionV0::at_timestamp(42, spec(), list))
            .unwrap();

        assert_eq!(
            relay_spec::bytemuck::bytes_of(&wrapped),
            relay_spec::bytemuck::bytes_of(&bare),
            "the wrapper must not perturb a single wire byte"
        );

        // And the block reads back through the canonical reader, at the
        // offset the field was initialized with.
        let mut account = vec![0u8; BLOCK_OFFSET as usize];
        account.extend_from_slice(relay_spec::bytemuck::bytes_of(&wrapped));
        let (header, conditions) = relay_spec::read_block(&account, BLOCK_OFFSET as usize).unwrap();
        assert_eq!(header.num_conditions, 3);
        assert_eq!(
            conditions[0].wake(),
            Ok(WakeView::AtTimestamp { unix_ts: 42 })
        );
        assert!(!conditions[1].is_active());
    }

    /// A stand-in for a future spec version: the wrapper hosts it with no
    /// change to this crate, which is the property the generic exists for.
    /// If hosting a `RelayBlockV1` ever needs an edit here, this test is
    /// wrong about the design and should be the thing that fails.
    #[repr(transparent)]
    #[derive(Clone, Copy, Debug)]
    struct FutureBlock([u8; 208]);

    impl Default for FutureBlock {
        fn default() -> Self {
            Self::zeroed()
        }
    }

    unsafe impl Zeroable for FutureBlock {}
    unsafe impl Pod for FutureBlock {}

    impl ConditionBlock for FutureBlock {
        const NUM_CONDITIONS: usize = 1;

        fn block(&self) -> &[u8] {
            &self.0
        }

        fn block_mut(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    impl RelayBlockVersion for FutureBlock {
        const SPEC_VERSION: u8 = 1;
        const SIZE: usize = 208;

        fn idl_name() -> String {
            "RelayBlockV1_1x0".to_string()
        }
    }

    #[test]
    fn the_wrapper_hosts_a_future_version_unchanged() {
        let mut hosted = RelayBlockHost::<FutureBlock>::zeroed();
        assert_eq!(core::mem::size_of_val(&hosted), FutureBlock::SIZE);
        // The condition surface works on it the same way, through the same
        // delegation.
        hosted.init_header().unwrap();
        hosted
            .write_condition(
                0,
                &ConditionV0::every_slots(9, spec(), ResolverListV0::new(0, 0)),
            )
            .unwrap();
        assert_eq!(hosted.read_condition(0).unwrap().wake_slot(), 9);
        assert_eq!(
            ConditionBlock::write_condition(
                &mut hosted,
                1,
                &ConditionV0::every_slots(1, spec(), ResolverListV0::new(0, 0))
            ),
            Err(SpecError::TooLarge),
            "one slot means one slot"
        );
    }

    #[cfg(feature = "idl-build")]
    #[test]
    fn the_idl_describes_the_region_as_sized_bytes() {
        use anchor_lang::idl::types::*;
        use anchor_lang::idl::IdlBuild;

        let ty = <Block as IdlBuild>::create_type().expect("a type def");
        assert_eq!(ty.name, "RelayBlock3x8");
        assert!(matches!(ty.serialization, IdlSerialization::BytemuckUnsafe));
        let IdlTypeDefTy::Struct {
            fields: Some(IdlDefinedFields::Named(fields)),
        } = ty.ty
        else {
            panic!("expected one named field");
        };
        assert_eq!(fields.len(), 1);
        assert!(matches!(
            &fields[0].ty,
            IdlType::Array(inner, IdlArrayLen::Value(len))
                if **inner == IdlType::U8 && *len == RelayBlockV0::<3, 8>::SIZE
        ));

        // A future version is named distinctly, so a client cannot decode
        // one version's bytes as another's.
        assert_eq!(
            <RelayBlockHost<FutureBlock> as IdlBuild>::get_full_path(),
            "RelayBlockV1_1x0"
        );
    }
}
