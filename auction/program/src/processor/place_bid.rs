//! Places a bid on a running auction, the logic here implements a standard English auction
//! mechanism, once the auction starts, new bids can be made until 10 minutes has passed with no
//! new bid. At this point the auction ends.
//!
//! Possible Attacks to Consider:
//!
//! 1) A user bids many many small bids to fill up the buffer, so that his max bid wins.
//! 2) A user bids a large amount repeatedly to indefinitely delay the auction finishing.
//!
//! A few solutions come to mind: don't allow cancelling bids, and simply prune all bids that
//! are not winning bids from the state.

use crate::{
    errors::AuctionError,
    processor::{AuctionData, AuctionState, Bid, BidderMetadata, BidderPot, PriceFloor},
    utils::{
        assert_derivation, assert_signer, assert_initialized, assert_owned_by, create_or_allocate_account_raw,
        spl_token_transfer, TokenTransferParams,
    },
    PREFIX,
};

use {
    borsh::{BorshDeserialize, BorshSerialize},
    solana_program::{
        account_info::{next_account_info, AccountInfo},
        borsh::try_from_slice_unchecked,
        entrypoint::ProgramResult,
        msg,
        program::{invoke, invoke_signed},
        program_pack::Pack,
        program_error::ProgramError,
        pubkey::Pubkey,
        rent::Rent,
        system_instruction,
        system_instruction::create_account,
        sysvar::{clock::Clock, Sysvar},
    },
    spl_token::state::Account,
    std::mem,
};

/// Arguments for the PlaceBid instruction discriminant .
#[repr(C)]
#[derive(Clone, BorshSerialize, BorshDeserialize, PartialEq)]
pub struct PlaceBidArgs {
    /// Size of the bid being placed. The user must have enough SOL to satisfy this amount.
    pub amount: u64,
    /// Resource being bid on.
    pub resource: Pubkey,
}

struct Accounts<'a, 'b: 'a> {
    auction: &'a AccountInfo<'b>,
    bidder_meta: &'a AccountInfo<'b>,
    bidder_pot: &'a AccountInfo<'b>,
    bidder_pot_token: &'a AccountInfo<'b>,
    bidder: &'a AccountInfo<'b>,
    clock_sysvar: &'a AccountInfo<'b>,
    mint: &'a AccountInfo<'b>,
    payer: &'a AccountInfo<'b>,
    rent: &'a AccountInfo<'b>,
    system: &'a AccountInfo<'b>,
    token_program: &'a AccountInfo<'b>,
    transfer_authority: &'a AccountInfo<'b>,
}

fn parse_accounts<'a, 'b: 'a>(
    program_id: &Pubkey,
    accounts: &'a [AccountInfo<'b>],
) -> Result<Accounts<'a, 'b>, ProgramError> {
    let account_iter = &mut accounts.iter();
    let accounts = Accounts {
        bidder: next_account_info(account_iter)?,
        bidder_pot: next_account_info(account_iter)?,
        bidder_pot_token: next_account_info(account_iter)?,
        bidder_meta: next_account_info(account_iter)?,
        auction: next_account_info(account_iter)?,
        mint: next_account_info(account_iter)?,
        transfer_authority: next_account_info(account_iter)?,
        payer: next_account_info(account_iter)?,
        clock_sysvar: next_account_info(account_iter)?,
        rent: next_account_info(account_iter)?,
        system: next_account_info(account_iter)?,
        token_program: next_account_info(account_iter)?,
    };

    assert_owned_by(accounts.auction, program_id)?;
    assert_owned_by(accounts.mint, &spl_token::id())?;
    assert_owned_by(accounts.bidder_pot_token, &spl_token::id())?;
    assert_signer(accounts.bidder)?;
    assert_signer(accounts.transfer_authority)?;

    Ok(accounts)
}

pub fn place_bid<'r, 'b: 'r>(
    program_id: &Pubkey,
    accounts: &'r [AccountInfo<'b>],
    args: PlaceBidArgs,
) -> ProgramResult {
    msg!("+ Processing PlaceBid");
    let accounts = parse_accounts(program_id, accounts)?;

    // Derive Metadata key and load it.
    let metadata_bump = assert_derivation(
        program_id,
        accounts.bidder_meta,
        &[
            PREFIX.as_bytes(),
            program_id.as_ref(),
            accounts.auction.key.as_ref(),
            accounts.bidder.key.as_ref(),
            "metadata".as_bytes(),
        ],
    )?;

    // If metadata doesn't exist, create it.
    let mut bidder_metadata: BidderMetadata =
        if accounts.bidder_meta.owner != program_id {
            create_or_allocate_account_raw(
                *program_id,
                accounts.bidder_meta,
                accounts.rent,
                accounts.system,
                accounts.payer,
                mem::size_of::<BidderMetadata>(),
                &[
                    PREFIX.as_bytes(),
                    program_id.as_ref(),
                    accounts.auction.key.as_ref(),
                    accounts.bidder.key.as_ref(),
                    "metadata".as_bytes(),
                    &[metadata_bump],
                ],
            )?;
            try_from_slice_unchecked(&accounts.bidder_meta.data.borrow_mut())?
        } else {
            // Verify the last bid was cancelled before continuing.
            let metadata: BidderMetadata = try_from_slice_unchecked(&accounts.bidder_meta.data.borrow_mut())?;
            if metadata.cancelled == false {
                return Err(AuctionError::BidAlreadyActive.into());
            }
            metadata
        };

    // Derive Pot address, this account wraps/holds an SPL account to transfer tokens into and is
    // also used as the authoriser of the SPL pot.
    let pot_bump = assert_derivation(
        program_id,
        accounts.bidder_pot,
        &[
            PREFIX.as_bytes(),
            program_id.as_ref(),
            accounts.auction.key.as_ref(),
            accounts.bidder.key.as_ref(),
        ],
    )?;

    // The account within the pot must be owned by us.
    let actual_account: Account = assert_initialized(accounts.bidder_pot_token)?;
    if actual_account.owner != *accounts.auction.key {
        return Err(AuctionError::BidderPotTokenAccountOwnerMismatch.into());
    }

    // Derive and load Auction.
    let auction_bump = assert_derivation(
        program_id,
        accounts.auction,
        &[
            PREFIX.as_bytes(),
            program_id.as_ref(),
            args.resource.as_ref(),
        ],
    )?;

    // Load the auction and verify this bid is valid.
    let mut auction: AuctionData = try_from_slice_unchecked(&accounts.auction.data.borrow())?;

    // The mint provided in this bid must match the one the auction was initialized with.
    if auction.token_mint != *accounts.mint.key {
        return Ok(());
    }

    // Can't bid on an auction that isn't running.
    if auction.state != AuctionState::Started {
        return Err(AuctionError::InvalidState.into());
    }

    // Can't bid smaller than the minimum price.
    if let PriceFloor::MinimumPrice(min) = auction.price_floor {
        if args.amount <= min {
            return Err(AuctionError::BidTooSmall.into());
        }
    }

    // Load the clock, used for various auction timing.
    let clock = Clock::from_account_info(accounts.clock_sysvar)?;

    // Verify auction has not ended.
    if auction.ended(clock.slot) {
        auction.state = auction.state.end()?;
        auction.serialize(&mut *accounts.auction.data.borrow_mut())?;
        return Ok(());
    }

    let bump_authority_seeds = &[
        PREFIX.as_bytes(),
        program_id.as_ref(),
        accounts.auction.key.as_ref(),
        accounts.bidder.key.as_ref(),
        &[pot_bump],
    ];

    // If the bidder pot account is empty, we need to generate one.
    if accounts.bidder_pot.data_is_empty() {
        create_or_allocate_account_raw(
            *program_id,
            accounts.bidder_pot,
            accounts.rent,
            accounts.system,
            accounts.payer,
            mem::size_of::<BidderPot>(),
            bump_authority_seeds,
        )?;

        // Attach SPL token address to pot account.
        let mut pot: BidderPot = try_from_slice_unchecked(&accounts.bidder_pot.data.borrow_mut())?;
        pot.bidder_pot = *accounts.bidder_pot_token.key;
        pot.bidder_act = *accounts.bidder.key;
        pot.auction_act = *accounts.auction.key;
        pot.serialize(&mut *accounts.bidder_pot.data.borrow_mut())?;
    } else {
        // Already exists, verify that the pot contains the specified SPL address.
        let bidder_pot: BidderPot = try_from_slice_unchecked(&accounts.bidder_pot.data.borrow_mut())?;
        if bidder_pot.bidder_pot != *accounts.bidder_pot_token.key {
            return Err(AuctionError::BidderPotTokenAccountOwnerMismatch.into());
        }
    }

    // Confirm payers SPL token balance is enough to pay the bid.
    let account: Account = Account::unpack_from_slice(&accounts.bidder.data.borrow())?;
    if account.amount.saturating_sub(args.amount) <= 0 {
        return Err(AuctionError::BalanceTooLow.into());
    }

    // Transfer amount of SPL token to bid account.
    spl_token_transfer(TokenTransferParams {
        source: accounts.bidder.clone(),
        destination: accounts.bidder_pot_token.clone(),
        authority: accounts.transfer_authority.clone(),
        authority_signer_seeds: bump_authority_seeds,
        token_program: accounts.token_program.clone(),
        amount: args.amount,
    })?;

    // Serialize new Auction State
    auction.last_bid = Some(clock.slot);
    auction
        .bid_state
        .place_bid(Bid(*accounts.bidder_pot.key, args.amount))?;
    auction.serialize(&mut *accounts.auction.data.borrow_mut())?;

    // Update latest metadata with results from the bid.
    BidderMetadata {
        bidder_pubkey: *accounts.bidder.key,
        auction_pubkey: *accounts.auction.key,
        last_bid: clock.slot,
        last_bid_timestamp: clock.unix_timestamp,
        cancelled: false,
    }.serialize(&mut *accounts.bidder_meta.data.borrow_mut())?;

    Ok(())
}
