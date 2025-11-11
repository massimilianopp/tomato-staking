use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Mint, Token, TokenAccount, Transfer},
};

declare_id!("11111111111111111111111111111111"); // remplacé à build

const SCALE: u128 = 1_000_000_000_000_000_000; // 1e18 pour précision

#[program]
pub mod tomato_staking {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        reward_rate_per_sec: u64,
    ) -> Result<()> {
        let state = &mut ctx.accounts.state;
        state.admin = ctx.accounts.admin.key();
        state.stake_mint = ctx.accounts.stake_mint.key();  // ex: TOMATO
        state.reward_mint = ctx.accounts.reward_mint.key(); // peut être le même mint
        state.vault = ctx.accounts.vault.key();
        state.reward_vault = ctx.accounts.reward_vault.key();
        state.reward_rate_per_sec = reward_rate_per_sec;
        state.last_update = Clock::get()?.unix_timestamp as u64;
        state.reward_per_token_stored = 0;
        state.total_staked = 0;
        Ok(())
    }

    pub fn set_reward_rate(ctx: Context<OnlyAdmin>, new_rate: u64) -> Result<()> {
        // avant de changer le taux, on “avance” l’index
        update_global(&mut ctx.accounts.state)?;
        ctx.accounts.state.reward_rate_per_sec = new_rate;
        Ok(())
    }

    /// L’admin envoie d’abord des tokens vers son ATA, puis appelle `fund_rewards`
    /// pour transférer au `reward_vault` du programme (réserve de récompenses).
    pub fn fund_rewards(ctx: Context<FundRewards>, amount: u64) -> Result<()> {
        let cpi_accounts = Transfer {
            from: ctx.accounts.admin_reward_ata.to_account_info(),
            to: ctx.accounts.reward_vault.to_account_info(),
            authority: ctx.accounts.admin.to_account_info(),
        };
        let cpi = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi, amount)?;
        Ok(())
    }

    pub fn stake(ctx: Context<Stake>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::ZeroAmount);
        // 1) avance les index et solde les rewards utilisateur
        update_global(&mut ctx.accounts.state)?;
        update_user(&ctx.accounts.state, &mut ctx.accounts.user)?;

        // 2) transfère les tokens de l’utilisateur vers le vault
        let cpi_accounts = Transfer {
            from: ctx.accounts.user_stake_ata.to_account_info(),
            to: ctx.accounts.vault.to_account_info(),
            authority: ctx.accounts.user_authority.to_account_info(),
        };
        let cpi = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi, amount)?;

        // 3) met à jour les montants stakés
        ctx.accounts.user.amount = ctx.accounts.user.amount.checked_add(amount).unwrap();
        ctx.accounts.state.total_staked = ctx.accounts.state.total_staked.checked_add(amount).unwrap();
        Ok(())
    }

    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        require!(amount > 0, StakingError::ZeroAmount);
        require!(ctx.accounts.user.amount >= amount, StakingError::InsufficientStake);

        update_global(&mut ctx.accounts.state)?;
        update_user(&ctx.accounts.state, &mut ctx.accounts.user)?;

        ctx.accounts.user.amount -= amount;
        ctx.accounts.state.total_staked -= amount;

        // transfert inverse: vault -> user
        // signer PDA du programme pour dépenser le vault ATA
        let seeds = &[b"state", ctx.accounts.state.stake_mint.as_ref(), &[ctx.accounts.state.bump]];
        let signer = &[&seeds[..]];

        let cpi_accounts = Transfer {
            from: ctx.accounts.vault.to_account_info(),
            to: ctx.accounts.user_stake_ata.to_account_info(),
            authority: ctx.accounts.state.to_account_info(), // PDA
        };
        let cpi = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::transfer(cpi, amount)?;
        Ok(())
    }

    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        update_global(&mut ctx.accounts.state)?;
        update_user(&ctx.accounts.state, &mut ctx.accounts.user)?;
        let reward = ctx.accounts.user.rewards_owed;
        require!(reward > 0, StakingError::NoRewards);
        ctx.accounts.user.rewards_owed = 0;

        // reward_vault -> user_reward_ata (PDA signe)
        let seeds = &[b"state", ctx.accounts.state.stake_mint.as_ref(), &[ctx.accounts.state.bump]];
        let signer = &[&seeds[..]];
        let cpi_accounts = Transfer {
            from: ctx.accounts.reward_vault.to_account_info(),
            to: ctx.accounts.user_reward_ata.to_account_info(),
            authority: ctx.accounts.state.to_account_info(),
        };
        let cpi = CpiContext::new_with_signer(ctx.accounts.token_program.to_account_info(), cpi_accounts, signer);
        token::transfer(cpi, reward)?;
        Ok(())
    }

    pub fn exit(ctx: Context<Exit>) -> Result<()> {
        let amount = ctx.accounts.user.amount;
        if amount > 0 {
            // réutilise withdraw
            withdraw(
                Context::new(
                    ctx.program_id,
                    Withdraw {
                        state: ctx.accounts.state.clone(),
                        user: ctx.accounts.user.clone(),
                        user_authority: ctx.accounts.user_authority.clone(),
                        user_stake_ata: ctx.accounts.user_stake_ata.clone(),
                        vault: ctx.accounts.vault.clone(),
                        token_program: ctx.accounts.token_program.clone(),
                    },
                    ctx.remaining_accounts.to_vec(),
                ),
                amount,
            )?;
        }
        claim(
            Context::new(
                ctx.program_id,
                Claim {
                    state: ctx.accounts.state.clone(),
                    user: ctx.accounts.user.clone(),
                    user_authority: ctx.accounts.user_authority.clone(),
                    user_reward_ata: ctx.accounts.user_reward_ata.clone(),
                    reward_vault: ctx.accounts.reward_vault.clone(),
                    token_program: ctx.accounts.token_program.clone(),
                },
                vec![],
            )
        )?;
        Ok(())
    }
}

/* ----------------------------- State & helpers ----------------------------- */

#[account]
pub struct GlobalState {
    pub admin: Pubkey,
    pub stake_mint: Pubkey,
    pub reward_mint: Pubkey,
    pub vault: Pubkey,         // ATA du PDA pour stake_mint
    pub reward_vault: Pubkey,  // ATA du PDA pour reward_mint
    pub reward_rate_per_sec: u64,
    pub last_update: u64,
    pub reward_per_token_stored: u128, // en 1e18
    pub total_staked: u64,
    pub bump: u8,
}

#[account]
pub struct UserStake {
    pub owner: Pubkey,
    pub amount: u64,
    pub user_reward_per_token_paid: u128,
    pub rewards_owed: u64,
}

fn update_global(state: &mut Account<GlobalState>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp as u64;
    if state.total_staked == 0 {
        state.last_update = now;
        return Ok(());
    }
    let elapsed = now - state.last_update;
    // rpt += elapsed * reward_rate * 1e18 / total_staked
    let add = (elapsed as u128)
        .checked_mul(state.reward_rate_per_sec as u128).unwrap()
        .checked_mul(SCALE).unwrap()
        / (state.total_staked as u128);
    state.reward_per_token_stored = state.reward_per_token_stored.checked_add(add).unwrap();
    state.last_update = now;
    Ok(())
}

fn update_user(state: &Account<GlobalState>, user: &mut Account<UserStake>) -> Result<()> {
    let delta = state.reward_per_token_stored - user.user_reward_per_token_paid;
    // earned = amount * delta / 1e18
    let earned = ((user.amount as u128)
        .checked_mul(delta).unwrap()
        / SCALE) as u64;
    user.rewards_owed = user.rewards_owed.checked_add(earned).unwrap();
    user.user_reward_per_token_paid = state.reward_per_token_stored;
    Ok(())
}

/* -------------------------------- Accounts -------------------------------- */

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    pub stake_mint: Account<'info, Mint>,
    pub reward_mint: Account<'info, Mint>,

    /// PDA = seeds ["state", stake_mint], payer admin
    #[account(
        init,
        payer = admin,
        space = 8 + 32*5 + 8*4 + 1 + 16, // marge
        seeds = [b"state", stake_mint.key().as_ref()],
        bump
    )]
    pub state: Account<'info, GlobalState>,

    /// ATA du PDA pour stake_mint
    #[account(
        init,
        payer = admin,
        associated_token::mint = stake_mint,
        associated_token::authority = state
    )]
    pub vault: Account<'info, TokenAccount>,

    /// ATA du PDA pour reward_mint
    #[account(
        init,
        payer = admin,
        associated_token::mint = reward_mint,
        associated_token::authority = state
    )]
    pub reward_vault: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct OnlyAdmin<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(
        mut,
        has_one = admin @ StakingError::NotAdmin,
    )]
    pub state: Account<'info, GlobalState>,
}

#[derive(Accounts)]
pub struct FundRewards<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(has_one = admin @ StakingError::NotAdmin)]
    pub state: Account<'info, GlobalState>,

    #[account(mut)]
    pub admin_reward_ata: Account<'info, TokenAccount>,
    /// CHECK: ATA appartient au PDA state
    #[account(mut, address = state.reward_vault)]
    pub reward_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub user_authority: Signer<'info>,

    #[account(mut)]
    pub state: Account<'info, GlobalState>,

    #[account(
        init_if_needed,
        payer = user_authority,
        space = 8 + 32 + 8 + 16 + 8,
        seeds = [b"user", state.key().as_ref(), user_authority.key().as_ref()],
        bump
    )]
    pub user: Account<'info, UserStake>,

    #[account(mut)]
    pub user_stake_ata: Account<'info, TokenAccount>,
    /// CHECK
    #[account(mut, address = state.vault)]
    pub vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub user_authority: Signer<'info>,
    #[account(mut)]
    pub state: Account<'info, GlobalState>,
    #[account(mut, seeds = [b"user", state.key().as_ref(), user_authority.key().as_ref()], bump)]
    pub user: Account<'info, UserStake>,

    #[account(mut)]
    pub user_stake_ata: Account<'info, TokenAccount>,
    /// CHECK
    #[account(mut, address = state.vault)]
    pub vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub user_authority: Signer<'info>,
    #[account(mut)]
    pub state: Account<'info, GlobalState>,
    #[account(mut, seeds = [b"user", state.key().as_ref(), user_authority.key().as_ref()], bump)]
    pub user: Account<'info, UserStake>,

    #[account(mut)]
    pub user_reward_ata: Account<'info, TokenAccount>,
    /// CHECK
    #[account(mut, address = state.reward_vault)]
    pub reward_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Exit<'info> {
    #[account(mut)]
    pub user_authority: Signer<'info>,
    #[account(mut)]
    pub state: Account<'info, GlobalState>,
    #[account(mut, seeds = [b"user", state.key().as_ref(), user_authority.key().as_ref()], bump)]
    pub user: Account<'info, UserStake>,

    #[account(mut)]
    pub user_stake_ata: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_reward_ata: Account<'info, TokenAccount>,
    /// CHECK
    #[account(mut, address = state.vault)]
    pub vault: Account<'info, TokenAccount>,
    /// CHECK
    #[account(mut, address = state.reward_vault)]
    pub reward_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[error_code]
pub enum StakingError {
    #[msg("Not admin")]
    NotAdmin,
    #[msg("Amount is zero")]
    ZeroAmount,
    #[msg("Insufficient stake")]
    InsufficientStake,
    #[msg("No rewards to claim")]
    NoRewards,
}
