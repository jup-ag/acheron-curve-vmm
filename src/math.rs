use jupiter_amm_interface::AmmError;

use crate::state::{CurveType, FeeBeneficiary, MAX_BENEFICIARIES};

const MAX_BPS: u16 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeeBreakdown {
    pub platform_fee: u64,
    pub beneficiary_fees: [u64; MAX_BENEFICIARIES],
    pub total_fee: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapComputation {
    pub new_reserves_a: u128,
    pub new_reserves_b: u128,
    pub amount_a: u64,
    pub amount_b: u64,
}

pub fn calculate_fee_breakdown(
    amount: u64,
    platform_fee_bps: u16,
    beneficiaries: &[FeeBeneficiary],
) -> Result<FeeBreakdown, AmmError> {
    if platform_fee_bps > MAX_BPS {
        return Err(AmmError::from("Scale VMM: invalid platform fee"));
    }
    let mut total_bps = u32::from(platform_fee_bps);
    for beneficiary in beneficiaries {
        if beneficiary.share_bps > MAX_BPS {
            return Err(AmmError::from("Scale VMM: invalid creator fee"));
        }
        total_bps = total_bps
            .checked_add(u32::from(beneficiary.share_bps))
            .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?;
    }
    if total_bps > u32::from(MAX_BPS) {
        return Err(AmmError::from("Scale VMM: invalid total fees"));
    }

    let mut breakdown = FeeBreakdown {
        platform_fee: 0,
        beneficiary_fees: [0u64; MAX_BENEFICIARIES],
        total_fee: 0,
    };

    breakdown.platform_fee = calculate_fee(amount, platform_fee_bps)?;
    breakdown.total_fee = breakdown
        .total_fee
        .checked_add(breakdown.platform_fee)
        .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?;

    for (idx, beneficiary) in beneficiaries.iter().enumerate() {
        let fee = calculate_fee(amount, beneficiary.share_bps)?;
        breakdown.beneficiary_fees[idx] = fee;
        breakdown.total_fee = breakdown
            .total_fee
            .checked_add(fee)
            .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?;
    }

    if breakdown.total_fee >= amount {
        return Err(AmmError::from("Scale VMM: insufficient input after fees"));
    }

    Ok(breakdown)
}

pub fn quote_buy(
    reserve_a_virtual: u128,
    reserve_b: u128,
    amount_a_after_fee: u64,
    curve: CurveType,
) -> Result<SwapComputation, AmmError> {
    if amount_a_after_fee == 0 {
        return Err(AmmError::from("Scale VMM: insufficient input"));
    }

    let input = amount_a_after_fee as u128;
    let output = apply_curve(input, reserve_a_virtual, reserve_b, curve)?;
    if output == 0 {
        return Err(AmmError::from("Scale VMM: insufficient output"));
    }

    let new_reserve_a_virtual = reserve_a_virtual
        .checked_add(input)
        .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?;
    let new_reserve_b = reserve_b
        .checked_sub(output)
        .ok_or_else(|| AmmError::from("Scale VMM: insufficient output"))?;

    Ok(SwapComputation {
        new_reserves_a: new_reserve_a_virtual,
        new_reserves_b: new_reserve_b,
        amount_a: amount_a_after_fee,
        amount_b: u64::try_from(output).map_err(|_| AmmError::from("Scale VMM: math overflow"))?,
    })
}

pub fn quote_sell(
    reserve_a_virtual: u128,
    reserve_b: u128,
    amount_b: u64,
    curve: CurveType,
) -> Result<SwapComputation, AmmError> {
    if amount_b == 0 {
        return Err(AmmError::from("Scale VMM: insufficient input"));
    }

    let input = amount_b as u128;
    let output = apply_curve(input, reserve_b, reserve_a_virtual, curve)?;
    if output == 0 {
        return Err(AmmError::from("Scale VMM: insufficient output"));
    }

    let new_reserve_b = reserve_b
        .checked_add(input)
        .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?;
    let new_reserve_a_virtual = reserve_a_virtual
        .checked_sub(output)
        .ok_or_else(|| AmmError::from("Scale VMM: insufficient output"))?;

    Ok(SwapComputation {
        new_reserves_a: new_reserve_a_virtual,
        new_reserves_b: new_reserve_b,
        amount_a: u64::try_from(output).map_err(|_| AmmError::from("Scale VMM: math overflow"))?,
        amount_b,
    })
}

fn apply_curve(
    input: u128,
    input_reserve: u128,
    output_reserve: u128,
    curve: CurveType,
) -> Result<u128, AmmError> {
    if input_reserve == 0 || output_reserve == 0 {
        return Err(AmmError::from("Scale VMM: pool empty"));
    }

    let denominator = match curve {
        CurveType::ConstantProduct => input_reserve
            .checked_add(input)
            .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?,
        CurveType::Exponential => {
            let scaled = input
                .checked_mul(3)
                .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?
                .checked_div(2)
                .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?;
            input_reserve
                .checked_add(scaled)
                .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?
        }
    };

    input
        .checked_mul(output_reserve)
        .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))?
        .checked_div(denominator)
        .ok_or_else(|| AmmError::from("Scale VMM: math overflow"))
}

fn calculate_fee(amount: u64, bps: u16) -> Result<u64, AmmError> {
    let fee = u128::from(amount)
        .checked_mul(u128::from(bps))
        .ok_or_else(|| AmmError::from("Scale VMM: fee overflow"))?
        .checked_div(u128::from(MAX_BPS))
        .ok_or_else(|| AmmError::from("Scale VMM: fee overflow"))?;
    u64::try_from(fee).map_err(|_| AmmError::from("Scale VMM: fee overflow"))
}
