use borsh::{BorshDeserialize, BorshSerialize};
use jupiter_amm_interface::AmmError;
use sha2::{Digest, Sha256};
use solana_pubkey::Pubkey;

pub const MAX_BENEFICIARIES: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshDeserialize, BorshSerialize, Default)]
pub struct FeeBeneficiary {
    pub wallet: Pubkey,
    pub share_bps: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, BorshDeserialize, BorshSerialize, Default)]
pub enum CurveType {
    #[default]
    ConstantProduct,
    Exponential,
}

#[derive(Clone, Debug, PartialEq, Eq, BorshDeserialize, BorshSerialize, Default)]
pub struct ScalePairState {
    pub enabled: bool,
    pub graduated: bool,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub token_a_reserves: u128,
    pub token_b_reserves: u128,
    pub shift: u128,
    pub curve: CurveType,
    pub fee_beneficiary_count: u8,
    pub fee_beneficiaries: [FeeBeneficiary; MAX_BENEFICIARIES],
    pub amm_pool: Pubkey,
    pub bump: u8,
}

impl ScalePairState {
    pub fn fee_beneficiaries(&self) -> &[FeeBeneficiary] {
        &self.fee_beneficiaries[..self.fee_beneficiary_count as usize]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, BorshDeserialize, BorshSerialize, Default)]
pub struct ScalePlatformConfig {
    pub authority: Pubkey,
    pub fee_beneficiary: Pubkey,
    pub base_token: Pubkey,
    pub platform_fee_bps: u16,
    pub graduation_threshold: u64,
    pub bump: u8,
}

pub fn anchor_discriminator(namespace: &str, name: &str) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update(b":");
    hasher.update(name.as_bytes());
    let hash = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash[..8]);
    out
}

fn decode_anchor_account<T: BorshDeserialize>(
    account_name: &str,
    data: &[u8],
) -> Result<T, AmmError> {
    if data.len() < 8 {
        return Err(AmmError::from(format!(
            "Scale VMM account {account_name} data is too short"
        )));
    }
    let expected = anchor_discriminator("account", account_name);
    if data[..8] != expected {
        return Err(AmmError::from(format!(
            "Invalid discriminator for Scale VMM account {account_name}"
        )));
    }
    T::try_from_slice(&data[8..]).map_err(|e| {
        AmmError::from(format!(
            "Failed to decode Scale VMM account {account_name}: {e}"
        ))
    })
}

pub fn decode_pair_account(data: &[u8]) -> Result<ScalePairState, AmmError> {
    decode_anchor_account("PairState", data)
}

pub fn decode_platform_config_account(data: &[u8]) -> Result<ScalePlatformConfig, AmmError> {
    decode_anchor_account("PlatformConfig", data)
}
