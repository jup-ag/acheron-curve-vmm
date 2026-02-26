use jupiter_amm_interface::{
    AccountProvider, Amm, AmmContext, AmmError, AmmLabel, AmmProgramIdToLabel, KeyedAccount, Quote,
    QuoteParams, Swap, SwapAndAccountMetas, SwapMode, SwapParams,
};
use rust_decimal::Decimal;
use solana_account::ReadableAccount;
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

use crate::constants::{
    ASSOCIATED_TOKEN_PROGRAM_ID, SCALE_AMM_PROGRAM_ID, SCALE_VMM_LABEL, SCALE_VMM_PROGRAM_ID,
    SPL_TOKEN_PROGRAM_ID, SYSTEM_PROGRAM_ID,
};
use crate::math::{calculate_fee_breakdown, quote_buy, quote_sell};
use crate::state::{
    ScalePairState, ScalePlatformConfig, decode_pair_account, decode_platform_config_account,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleSwapLeg {
    TokenSwap,
    Gamma,
    MeteoraDammV2,
    Obsidian,
    RaydiumV2,
}

impl Default for ScaleSwapLeg {
    fn default() -> Self {
        Self::TokenSwap
    }
}

impl ScaleSwapLeg {
    fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "tokenswap" | "token_swap" => Some(Self::TokenSwap),
            "gamma" => Some(Self::Gamma),
            "meteoradammv2" | "meteora_damm_v2" => Some(Self::MeteoraDammV2),
            "obsidian" => Some(Self::Obsidian),
            "raydiumv2" | "raydium_v2" => Some(Self::RaydiumV2),
            _ => None,
        }
    }

    fn from_params(params: Option<&serde_json::Value>) -> Result<Self, AmmError> {
        let Some(params) = params else {
            return Ok(Self::default());
        };

        let maybe_swap_name = params
            .get("swap")
            .or_else(|| params.get("swap_variant"))
            .and_then(serde_json::Value::as_str);

        let Some(swap_name) = maybe_swap_name else {
            return Ok(Self::default());
        };

        Self::from_name(swap_name)
            .ok_or_else(|| AmmError::from(format!("Unsupported swap leg in params: {swap_name}")))
    }

    fn as_swap(self) -> Swap {
        match self {
            Self::TokenSwap => Swap::TokenSwap,
            Self::Gamma => Swap::Gamma,
            Self::MeteoraDammV2 => Swap::MeteoraDammV2,
            Self::Obsidian => Swap::Obsidian,
            Self::RaydiumV2 => Swap::RaydiumV2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    BuyAtoB,
    SellBtoA,
}

#[derive(Clone, Debug)]
pub struct ScaleVmm {
    key: Pubkey,
    pair: ScalePairState,
    config_address: Pubkey,
    config: ScalePlatformConfig,
    token_program_a: Pubkey,
    token_program_b: Pubkey,
    amm_program_id: Pubkey,
    swap_leg: ScaleSwapLeg,
    is_ready: bool,
}

impl ScaleVmm {
    fn direction_for_mints(
        &self,
        input_mint: Pubkey,
        output_mint: Pubkey,
    ) -> Result<Direction, AmmError> {
        if input_mint == self.pair.mint_a && output_mint == self.pair.mint_b {
            return Ok(Direction::BuyAtoB);
        }
        if input_mint == self.pair.mint_b && output_mint == self.pair.mint_a {
            return Ok(Direction::SellBtoA);
        }
        Err(AmmError::from(format!(
            "Scale VMM pair {} does not support mint pair {} -> {}",
            self.key, input_mint, output_mint
        )))
    }

    fn ensure_ready(&self) -> Result<(), AmmError> {
        if !self.is_ready {
            return Err(AmmError::from(
                "Scale VMM is not updated yet; call update before quoting/swapping",
            ));
        }
        Ok(())
    }

    fn ensure_exact_in(swap_mode: SwapMode) -> Result<(), AmmError> {
        if swap_mode == SwapMode::ExactOut {
            return Err(AmmError::from("Scale VMM does not support ExactOut"));
        }
        Ok(())
    }

    fn get_config_address(program_id: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[b"config"], program_id).0
    }

    fn get_vmm_vault_address(pair: &Pubkey, mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[pair.as_ref(), mint.as_ref()], &SCALE_VMM_PROGRAM_ID).0
    }

    fn get_amm_pool_address(&self) -> Pubkey {
        if self.pair.amm_pool != Pubkey::default() {
            return self.pair.amm_pool;
        }
        Pubkey::find_program_address(
            &[
                b"pool",
                self.key.as_ref(),
                self.pair.mint_a.as_ref(),
                self.pair.mint_b.as_ref(),
            ],
            &self.amm_program_id,
        )
        .0
    }

    fn get_amm_vault_address(&self, amm_pool: &Pubkey, mint: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[amm_pool.as_ref(), mint.as_ref()], &self.amm_program_id).0
    }

    fn get_ata(owner: &Pubkey, mint: &Pubkey, token_program_id: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(
            &[owner.as_ref(), token_program_id.as_ref(), mint.as_ref()],
            &ASSOCIATED_TOKEN_PROGRAM_ID,
        )
        .0
    }

    fn fee_pct(fee_amount: u64, base_amount: u64) -> Decimal {
        if base_amount == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(fee_amount) / Decimal::from(base_amount)
    }

    fn parse_amm_program_id(params: Option<&serde_json::Value>) -> Result<Pubkey, AmmError> {
        let Some(params) = params else {
            return Ok(SCALE_AMM_PROGRAM_ID);
        };

        let maybe_program_id = params
            .get("amm_program_id")
            .or_else(|| params.get("ammProgramId"))
            .and_then(serde_json::Value::as_str);

        let Some(program_id_str) = maybe_program_id else {
            return Ok(SCALE_AMM_PROGRAM_ID);
        };

        program_id_str.parse::<Pubkey>().map_err(|_| {
            AmmError::from(format!(
                "Invalid amm_program_id in params: {program_id_str}"
            ))
        })
    }
}

impl AmmProgramIdToLabel for ScaleVmm {
    const PROGRAM_ID_TO_LABELS: &[(Pubkey, AmmLabel)] = &[(SCALE_VMM_PROGRAM_ID, SCALE_VMM_LABEL)];
}

impl Amm for ScaleVmm {
    fn from_keyed_account(
        keyed_account: &KeyedAccount,
        _amm_context: &AmmContext,
    ) -> Result<Self, AmmError>
    where
        Self: Sized,
    {
        if keyed_account.account.owner != SCALE_VMM_PROGRAM_ID {
            return Err(AmmError::from(format!(
                "Unexpected owner {} for Scale VMM pair {}",
                keyed_account.account.owner, keyed_account.key
            )));
        }

        let pair = decode_pair_account(&keyed_account.account.data)?;
        let swap_leg = ScaleSwapLeg::from_params(keyed_account.params.as_ref())?;
        let amm_program_id = Self::parse_amm_program_id(keyed_account.params.as_ref())?;

        Ok(Self {
            key: keyed_account.key,
            pair,
            config_address: Self::get_config_address(&SCALE_VMM_PROGRAM_ID),
            config: ScalePlatformConfig::default(),
            token_program_a: SPL_TOKEN_PROGRAM_ID,
            token_program_b: SPL_TOKEN_PROGRAM_ID,
            amm_program_id,
            swap_leg,
            is_ready: false,
        })
    }

    fn label(&self) -> AmmLabel {
        SCALE_VMM_LABEL
    }

    fn program_id(&self) -> Pubkey {
        SCALE_VMM_PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.pair.mint_a, self.pair.mint_b]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![
            self.key,
            self.config_address,
            self.pair.mint_a,
            self.pair.mint_b,
        ]
    }

    fn update(&mut self, account_provider: impl AccountProvider) -> Result<(), AmmError> {
        let pair_account = account_provider.try_get(&self.key)?;
        self.pair = decode_pair_account(pair_account.data())?;

        let config_account = account_provider.try_get(&self.config_address)?;
        self.config = decode_platform_config_account(config_account.data())?;

        let mint_a_account = account_provider.try_get(&self.pair.mint_a)?;
        self.token_program_a = *mint_a_account.owner();

        let mint_b_account = account_provider.try_get(&self.pair.mint_b)?;
        self.token_program_b = *mint_b_account.owner();

        self.is_ready = true;
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote, AmmError> {
        Self::ensure_exact_in(quote_params.swap_mode)?;
        self.ensure_ready()?;

        if !self.pair.enabled {
            return Err(AmmError::from("Scale VMM pair is disabled"));
        }

        let reserve_a_virtual = self
            .pair
            .token_a_reserves
            .checked_add(self.pair.shift)
            .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?;

        if reserve_a_virtual == 0 || self.pair.token_b_reserves == 0 {
            return Err(AmmError::from("Scale VMM: pool empty"));
        }

        let direction =
            self.direction_for_mints(quote_params.input_mint, quote_params.output_mint)?;
        let beneficiaries = self.pair.fee_beneficiaries();

        match direction {
            Direction::BuyAtoB => {
                let fee_breakdown = calculate_fee_breakdown(
                    quote_params.amount,
                    self.config.platform_fee_bps,
                    beneficiaries,
                )?;
                let amount_after_fee = quote_params
                    .amount
                    .checked_sub(fee_breakdown.total_fee)
                    .ok_or_else(|| AmmError::from("Scale VMM: insufficient input"))?;
                let swap = quote_buy(
                    reserve_a_virtual,
                    self.pair.token_b_reserves,
                    amount_after_fee,
                    self.pair.curve,
                )?;
                Ok(Quote {
                    in_amount: quote_params.amount,
                    out_amount: swap.amount_b,
                    fee_amount: fee_breakdown.total_fee,
                    fee_mint: self.pair.mint_a,
                    fee_pct: Self::fee_pct(fee_breakdown.total_fee, quote_params.amount),
                })
            }
            Direction::SellBtoA => {
                let swap = quote_sell(
                    reserve_a_virtual,
                    self.pair.token_b_reserves,
                    quote_params.amount,
                    self.pair.curve,
                )?;
                let fee_breakdown = calculate_fee_breakdown(
                    swap.amount_a,
                    self.config.platform_fee_bps,
                    beneficiaries,
                )?;
                let amount_after_fee = swap
                    .amount_a
                    .checked_sub(fee_breakdown.total_fee)
                    .ok_or_else(|| AmmError::from("Scale VMM: insufficient output"))?;
                Ok(Quote {
                    in_amount: quote_params.amount,
                    out_amount: amount_after_fee,
                    fee_amount: fee_breakdown.total_fee,
                    fee_mint: self.pair.mint_a,
                    fee_pct: Self::fee_pct(fee_breakdown.total_fee, swap.amount_a),
                })
            }
        }
    }

    fn get_swap_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<SwapAndAccountMetas, AmmError> {
        Self::ensure_exact_in(swap_params.swap_mode)?;
        self.ensure_ready()?;

        let direction =
            self.direction_for_mints(swap_params.source_mint, swap_params.destination_mint)?;
        let (user_ta_a, user_ta_b) = match direction {
            Direction::BuyAtoB => (
                swap_params.source_token_account,
                swap_params.destination_token_account,
            ),
            Direction::SellBtoA => (
                swap_params.destination_token_account,
                swap_params.source_token_account,
            ),
        };

        let vmm_vault_a = Self::get_vmm_vault_address(&self.key, &self.pair.mint_a);
        let vmm_vault_b = Self::get_vmm_vault_address(&self.key, &self.pair.mint_b);
        let platform_fee_ta_a = Self::get_ata(
            &self.config.fee_beneficiary,
            &self.pair.mint_a,
            &self.token_program_a,
        );

        let amm_pool = self.get_amm_pool_address();
        let amm_vault_a = self.get_amm_vault_address(&amm_pool, &self.pair.mint_a);
        let amm_vault_b = self.get_amm_vault_address(&amm_pool, &self.pair.mint_b);
        let amm_config = Self::get_config_address(&self.amm_program_id);

        let mut account_metas = Vec::with_capacity(self.get_accounts_len());
        account_metas.push(AccountMeta::new_readonly(SCALE_VMM_PROGRAM_ID, false));
        account_metas.push(AccountMeta::new(self.key, false));
        account_metas.push(AccountMeta::new(swap_params.token_transfer_authority, true));
        account_metas.push(AccountMeta::new(self.pair.mint_a, false));
        account_metas.push(AccountMeta::new_readonly(self.pair.mint_b, false));
        account_metas.push(AccountMeta::new(user_ta_a, false));
        account_metas.push(AccountMeta::new(user_ta_b, false));
        account_metas.push(AccountMeta::new(vmm_vault_a, false));
        account_metas.push(AccountMeta::new(vmm_vault_b, false));
        account_metas.push(AccountMeta::new(platform_fee_ta_a, false));
        account_metas.push(AccountMeta::new_readonly(self.token_program_a, false));
        account_metas.push(AccountMeta::new_readonly(self.token_program_b, false));
        account_metas.push(AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false));
        account_metas.push(AccountMeta::new_readonly(self.config_address, false));
        account_metas.push(AccountMeta::new_readonly(self.amm_program_id, false));
        account_metas.push(AccountMeta::new(amm_pool, false));
        account_metas.push(AccountMeta::new(amm_vault_a, false));
        account_metas.push(AccountMeta::new(amm_vault_b, false));
        account_metas.push(AccountMeta::new_readonly(amm_config, false));
        account_metas.push(AccountMeta::new_readonly(self.token_program_a, false));
        account_metas.push(AccountMeta::new_readonly(self.token_program_b, false));
        account_metas.push(AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false));

        for beneficiary in self.pair.fee_beneficiaries() {
            let beneficiary_ata = Self::get_ata(
                &beneficiary.wallet,
                &self.pair.mint_a,
                &self.token_program_a,
            );
            account_metas.push(AccountMeta::new(beneficiary_ata, false));
        }

        Ok(SwapAndAccountMetas {
            swap: self.swap_leg.as_swap(),
            account_metas,
        })
    }

    fn has_dynamic_accounts(&self) -> bool {
        true
    }

    fn supports_exact_out(&self) -> bool {
        false
    }

    fn get_accounts_len(&self) -> usize {
        22 + self.pair.fee_beneficiaries().len()
    }

    fn is_active(&self) -> bool {
        self.pair.enabled
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use borsh::BorshSerialize;
    use jupiter_amm_interface::{Amm, AmmContext, FeeMode, KeyedAccount, QuoteParams};
    use solana_account::Account;
    use solana_pubkey::Pubkey;

    use crate::constants::{SCALE_AMM_PROGRAM_ID, SPL_TOKEN_PROGRAM_ID};
    use crate::state::{
        CurveType, FeeBeneficiary, ScalePairState, ScalePlatformConfig, anchor_discriminator,
    };

    use super::{ScaleSwapLeg, ScaleVmm};

    fn new_account(owner: Pubkey, data: Vec<u8>) -> Account {
        Account {
            lamports: 1,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn encode_anchor_account<T: BorshSerialize>(account_name: &str, value: &T) -> Vec<u8> {
        let mut data = anchor_discriminator("account", account_name).to_vec();
        data.extend(borsh::to_vec(value).unwrap());
        data
    }

    fn sample_pair(curve: CurveType) -> ScalePairState {
        let beneficiary_a = FeeBeneficiary {
            wallet: Pubkey::new_unique(),
            share_bps: 200,
        };
        let beneficiary_b = FeeBeneficiary {
            wallet: Pubkey::new_unique(),
            share_bps: 50,
        };
        let mut fee_beneficiaries = [FeeBeneficiary::default(); 5];
        fee_beneficiaries[0] = beneficiary_a;
        fee_beneficiaries[1] = beneficiary_b;

        ScalePairState {
            enabled: true,
            graduated: false,
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
            token_a_reserves: 1_000_000,
            token_b_reserves: 2_000_000,
            shift: 500_000,
            curve,
            fee_beneficiary_count: 2,
            fee_beneficiaries,
            amm_pool: Pubkey::default(),
            bump: 250,
        }
    }

    fn sample_config() -> ScalePlatformConfig {
        ScalePlatformConfig {
            authority: Pubkey::new_unique(),
            fee_beneficiary: Pubkey::new_unique(),
            base_token: Pubkey::new_unique(),
            platform_fee_bps: 100,
            graduation_threshold: 1_000_000_000,
            bump: 42,
        }
    }

    fn keyed_pair_account(pair_key: Pubkey, pair: &ScalePairState) -> KeyedAccount {
        KeyedAccount {
            key: pair_key,
            account: new_account(
                super::SCALE_VMM_PROGRAM_ID,
                encode_anchor_account("PairState", pair),
            ),
            params: None,
        }
    }

    fn update_map(
        pair_key: Pubkey,
        pair: &ScalePairState,
        config: &ScalePlatformConfig,
    ) -> HashMap<Pubkey, Arc<Account>> {
        let config_key = Pubkey::find_program_address(&[b"config"], &super::SCALE_VMM_PROGRAM_ID).0;
        HashMap::from([
            (
                pair_key,
                Arc::new(new_account(
                    super::SCALE_VMM_PROGRAM_ID,
                    encode_anchor_account("PairState", pair),
                )),
            ),
            (
                config_key,
                Arc::new(new_account(
                    super::SCALE_VMM_PROGRAM_ID,
                    encode_anchor_account("PlatformConfig", config),
                )),
            ),
            (
                pair.mint_a,
                Arc::new(new_account(SPL_TOKEN_PROGRAM_ID, Vec::new())),
            ),
            (
                pair.mint_b,
                Arc::new(new_account(SPL_TOKEN_PROGRAM_ID, Vec::new())),
            ),
        ])
    }

    #[test]
    fn rejects_invalid_pair_discriminator() {
        let pair_key = Pubkey::new_unique();
        let keyed = KeyedAccount {
            key: pair_key,
            account: new_account(super::SCALE_VMM_PROGRAM_ID, vec![0u8; 16]),
            params: None,
        };
        let err = ScaleVmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap_err();
        assert!(err.to_string().contains("Invalid discriminator"));
    }

    #[test]
    fn quote_buy_and_sell_constant_product() {
        let pair_key = Pubkey::new_unique();
        let pair = sample_pair(CurveType::ConstantProduct);
        let config = sample_config();
        let keyed = keyed_pair_account(pair_key, &pair);
        let mut amm = ScaleVmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap();
        amm.update(&update_map(pair_key, &pair, &config)).unwrap();

        let buy_quote = amm
            .quote(&QuoteParams {
                amount: 100_000,
                input_mint: pair.mint_a,
                output_mint: pair.mint_b,
                swap_mode: jupiter_amm_interface::SwapMode::ExactIn,
                fee_mode: FeeMode::Normal,
            })
            .unwrap();
        assert_eq!(buy_quote.in_amount, 100_000);
        assert_eq!(buy_quote.out_amount, 120_889);
        assert_eq!(buy_quote.fee_amount, 3_500);
        assert_eq!(buy_quote.fee_mint, pair.mint_a);

        let sell_quote = amm
            .quote(&QuoteParams {
                amount: 50_000,
                input_mint: pair.mint_b,
                output_mint: pair.mint_a,
                swap_mode: jupiter_amm_interface::SwapMode::ExactIn,
                fee_mode: FeeMode::Normal,
            })
            .unwrap();
        assert_eq!(sell_quote.in_amount, 50_000);
        assert_eq!(sell_quote.out_amount, 35_307);
        assert_eq!(sell_quote.fee_amount, 1_278);
        assert_eq!(sell_quote.fee_mint, pair.mint_a);
    }

    #[test]
    fn quote_buy_exponential_curve() {
        let pair_key = Pubkey::new_unique();
        let pair = sample_pair(CurveType::Exponential);
        let config = sample_config();
        let keyed = keyed_pair_account(pair_key, &pair);
        let mut amm = ScaleVmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap();
        amm.update(&update_map(pair_key, &pair, &config)).unwrap();

        let quote = amm
            .quote(&QuoteParams {
                amount: 100_000,
                input_mint: pair.mint_a,
                output_mint: pair.mint_b,
                swap_mode: jupiter_amm_interface::SwapMode::ExactIn,
                fee_mode: FeeMode::Normal,
            })
            .unwrap();
        assert_eq!(quote.out_amount, 117_343);
        assert_eq!(quote.fee_amount, 3_500);
    }

    #[test]
    fn exact_out_is_rejected() {
        let pair_key = Pubkey::new_unique();
        let pair = sample_pair(CurveType::ConstantProduct);
        let config = sample_config();
        let keyed = keyed_pair_account(pair_key, &pair);
        let mut amm = ScaleVmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap();
        amm.update(&update_map(pair_key, &pair, &config)).unwrap();

        let err = amm
            .quote(&QuoteParams {
                amount: 100_000,
                input_mint: pair.mint_a,
                output_mint: pair.mint_b,
                swap_mode: jupiter_amm_interface::SwapMode::ExactOut,
                fee_mode: FeeMode::Normal,
            })
            .unwrap_err();
        assert!(err.to_string().contains("ExactOut"));
    }

    #[test]
    fn invalid_fee_configuration_is_rejected() {
        let pair_key = Pubkey::new_unique();
        let mut pair = sample_pair(CurveType::ConstantProduct);
        pair.fee_beneficiary_count = 0;
        let mut config = sample_config();
        config.platform_fee_bps = 10_000;
        let keyed = keyed_pair_account(pair_key, &pair);
        let mut amm = ScaleVmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap();
        amm.update(&update_map(pair_key, &pair, &config)).unwrap();

        let err = amm
            .quote(&QuoteParams {
                amount: 100_000,
                input_mint: pair.mint_a,
                output_mint: pair.mint_b,
                swap_mode: jupiter_amm_interface::SwapMode::ExactIn,
                fee_mode: FeeMode::Normal,
            })
            .unwrap_err();
        assert!(err.to_string().contains("insufficient input after fees"));
    }

    #[test]
    fn builds_expected_account_metas_with_dynamic_beneficiaries_and_amm_accounts() {
        let pair_key = Pubkey::new_unique();
        let pair = sample_pair(CurveType::ConstantProduct);
        let config = sample_config();
        let keyed = keyed_pair_account(pair_key, &pair);
        let mut amm = ScaleVmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap();
        amm.update(&update_map(pair_key, &pair, &config)).unwrap();

        let source_token_account = Pubkey::new_unique();
        let destination_token_account = Pubkey::new_unique();
        let transfer_authority = Pubkey::new_unique();
        let jupiter_program_id = Pubkey::new_unique();

        let swap = amm
            .get_swap_and_account_metas(&jupiter_amm_interface::SwapParams {
                swap_mode: jupiter_amm_interface::SwapMode::ExactIn,
                in_amount: 100_000,
                out_amount: 120_000,
                source_mint: pair.mint_a,
                destination_mint: pair.mint_b,
                source_token_account,
                destination_token_account,
                token_transfer_authority: transfer_authority,
                user: Pubkey::new_unique(),
                payer: Pubkey::new_unique(),
                quote_mint_to_referrer: None,
                jupiter_program_id: &jupiter_program_id,
                missing_dynamic_accounts_as_default: false,
            })
            .unwrap();

        assert_eq!(swap.swap, jupiter_amm_interface::Swap::TokenSwap);
        assert_eq!(swap.account_metas.len(), 24);
        assert_eq!(swap.account_metas[0].pubkey, super::SCALE_VMM_PROGRAM_ID);
        assert_eq!(swap.account_metas[1].pubkey, pair_key);
        assert!(swap.account_metas[1].is_writable);
        assert_eq!(swap.account_metas[2].pubkey, transfer_authority);
        assert!(swap.account_metas[2].is_signer);
        assert_eq!(swap.account_metas[5].pubkey, source_token_account);
        assert_eq!(swap.account_metas[6].pubkey, destination_token_account);
        assert_eq!(swap.account_metas[10].pubkey, SPL_TOKEN_PROGRAM_ID);
        assert_eq!(swap.account_metas[11].pubkey, SPL_TOKEN_PROGRAM_ID);
        assert_eq!(swap.account_metas[14].pubkey, SCALE_AMM_PROGRAM_ID);

        let expected_amm_pool = Pubkey::find_program_address(
            &[
                b"pool",
                pair_key.as_ref(),
                pair.mint_a.as_ref(),
                pair.mint_b.as_ref(),
            ],
            &SCALE_AMM_PROGRAM_ID,
        )
        .0;
        assert_eq!(swap.account_metas[15].pubkey, expected_amm_pool);

        let expected_platform_fee_ata = Pubkey::find_program_address(
            &[
                config.fee_beneficiary.as_ref(),
                SPL_TOKEN_PROGRAM_ID.as_ref(),
                pair.mint_a.as_ref(),
            ],
            &super::ASSOCIATED_TOKEN_PROGRAM_ID,
        )
        .0;
        assert_eq!(swap.account_metas[9].pubkey, expected_platform_fee_ata);

        for (index, beneficiary) in pair.fee_beneficiaries().iter().enumerate() {
            let expected_ata = Pubkey::find_program_address(
                &[
                    beneficiary.wallet.as_ref(),
                    SPL_TOKEN_PROGRAM_ID.as_ref(),
                    pair.mint_a.as_ref(),
                ],
                &super::ASSOCIATED_TOKEN_PROGRAM_ID,
            )
            .0;
            assert_eq!(swap.account_metas[22 + index].pubkey, expected_ata);
            assert!(swap.account_metas[22 + index].is_writable);
        }
    }

    #[test]
    fn supports_params_override_for_swap_leg_and_amm_program() {
        let pair_key = Pubkey::new_unique();
        let pair = sample_pair(CurveType::ConstantProduct);
        let custom_amm_program = Pubkey::new_unique();

        let keyed = KeyedAccount {
            key: pair_key,
            account: new_account(
                super::SCALE_VMM_PROGRAM_ID,
                encode_anchor_account("PairState", &pair),
            ),
            params: Some(serde_json::json!({
                "swap": "gamma",
                "amm_program_id": custom_amm_program.to_string()
            })),
        };

        let amm = ScaleVmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap();
        assert_eq!(amm.swap_leg, ScaleSwapLeg::Gamma);
        assert_eq!(amm.amm_program_id, custom_amm_program);
    }
}
