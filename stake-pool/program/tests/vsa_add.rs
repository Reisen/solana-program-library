#![cfg(feature = "test-bpf")]

mod helpers;

use {
    bincode::deserialize,
    borsh::BorshSerialize,
    helpers::*,
    solana_program::{
        hash::Hash,
        instruction::{AccountMeta, Instruction, InstructionError},
        pubkey::Pubkey,
        sysvar,
    },
    solana_program_test::*,
    solana_sdk::{
        signature::{Keypair, Signer},
        transaction::{Transaction, TransactionError},
        transport::TransportError,
    },
    spl_stake_pool::{
        borsh::try_from_slice_unchecked, error, id, instruction, stake_program, state,
    },
};

async fn setup() -> (
    BanksClient,
    Keypair,
    Hash,
    StakePoolAccounts,
    ValidatorStakeAccount,
    Keypair,
) {
    let (mut banks_client, payer, recent_blockhash) = program_test().start().await;
    let stake_pool_accounts = StakePoolAccounts::new();
    stake_pool_accounts
        .initialize_stake_pool(&mut banks_client, &payer, &recent_blockhash)
        .await
        .unwrap();

    let user = Keypair::new();

    let user_stake = ValidatorStakeAccount::new_with_target_authority(
        &stake_pool_accounts.deposit_authority,
        &stake_pool_accounts.stake_pool.pubkey(),
    );
    user_stake
        .create_and_delegate(
            &mut banks_client,
            &payer,
            &recent_blockhash,
            &stake_pool_accounts.staker,
        )
        .await;

    // make pool token account
    let user_pool_account = Keypair::new();
    create_token_account(
        &mut banks_client,
        &payer,
        &recent_blockhash,
        &user_pool_account,
        &stake_pool_accounts.pool_mint.pubkey(),
        &user.pubkey(),
    )
    .await
    .unwrap();

    (
        banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    )
}

#[tokio::test]
async fn test_add_validator_to_pool() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    let error = stake_pool_accounts
        .add_validator_to_pool(
            &mut banks_client,
            &payer,
            &recent_blockhash,
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
        )
        .await;
    assert!(error.is_none());

    let stake_lamports = banks_client
        .get_account(user_stake.stake_account)
        .await
        .unwrap()
        .unwrap()
        .lamports;
    let deposit_tokens = stake_lamports; // For now 1:1 math
                                         // Check token account balance
    let token_balance = get_token_balance(&mut banks_client, &user_pool_account.pubkey()).await;
    assert_eq!(token_balance, deposit_tokens);
    let pool_fee_token_balance = get_token_balance(
        &mut banks_client,
        &stake_pool_accounts.pool_fee_account.pubkey(),
    )
    .await;
    assert_eq!(pool_fee_token_balance, 0); // No fee when adding validator stake accounts

    // Check if validator account was added to the list
    let validator_list = get_account(
        &mut banks_client,
        &stake_pool_accounts.validator_list.pubkey(),
    )
    .await;
    let validator_list =
        try_from_slice_unchecked::<state::ValidatorList>(validator_list.data.as_slice()).unwrap();
    assert_eq!(
        validator_list,
        state::ValidatorList {
            account_type: state::AccountType::ValidatorList,
            max_validators: stake_pool_accounts.max_validators,
            validators: vec![state::ValidatorStakeInfo {
                vote_account: user_stake.vote.pubkey(),
                last_update_epoch: 0,
                stake_lamports,
            }]
        }
    );

    // Check of stake account authority has changed
    let stake = get_account(&mut banks_client, &user_stake.stake_account).await;
    let stake_state = deserialize::<stake_program::StakeState>(&stake.data).unwrap();
    match stake_state {
        stake_program::StakeState::Stake(meta, _) => {
            assert_eq!(
                &meta.authorized.staker,
                &stake_pool_accounts.withdraw_authority
            );
            assert_eq!(
                &meta.authorized.withdrawer,
                &stake_pool_accounts.withdraw_authority
            );
        }
        _ => panic!(),
    }
}

#[tokio::test]
async fn test_add_validator_to_pool_with_wrong_token_program_id() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    let mut transaction = Transaction::new_with_payer(
        &[instruction::add_validator_to_pool(
            &id(),
            &stake_pool_accounts.stake_pool.pubkey(),
            &stake_pool_accounts.staker.pubkey(),
            &stake_pool_accounts.deposit_authority,
            &stake_pool_accounts.withdraw_authority,
            &stake_pool_accounts.validator_list.pubkey(),
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
            &stake_pool_accounts.pool_mint.pubkey(),
            &stake_program::id(),
        )
        .unwrap()],
        Some(&payer.pubkey()),
    );
    transaction.sign(&[&payer, &stake_pool_accounts.staker], recent_blockhash);
    let transaction_error = banks_client
        .process_transaction(transaction)
        .await
        .err()
        .unwrap();

    match transaction_error {
        TransportError::TransactionError(TransactionError::InstructionError(_, error)) => {
            assert_eq!(error, InstructionError::IncorrectProgramId);
        }
        _ => panic!("Wrong error occurs while try to add validator stake address with wrong token program ID"),
    }
}

#[tokio::test]
async fn test_add_validator_to_pool_with_wrong_pool_mint_account() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    let wrong_pool_mint = Keypair::new();

    let mut transaction = Transaction::new_with_payer(
        &[instruction::add_validator_to_pool(
            &id(),
            &stake_pool_accounts.stake_pool.pubkey(),
            &stake_pool_accounts.staker.pubkey(),
            &stake_pool_accounts.deposit_authority,
            &stake_pool_accounts.withdraw_authority,
            &stake_pool_accounts.validator_list.pubkey(),
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
            &wrong_pool_mint.pubkey(),
            &spl_token::id(),
        )
        .unwrap()],
        Some(&payer.pubkey()),
    );
    transaction.sign(&[&payer, &stake_pool_accounts.staker], recent_blockhash);
    let transaction_error = banks_client
        .process_transaction(transaction)
        .await
        .err()
        .unwrap();

    match transaction_error {
        TransportError::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(error_index),
        )) => {
            let program_error = error::StakePoolError::WrongPoolMint as u32;
            assert_eq!(error_index, program_error);
        }
        _ => panic!("Wrong error occurs while try to add validator stake address with wrong pool mint account"),
    }
}

#[tokio::test]
async fn test_add_validator_to_pool_with_wrong_validator_list_account() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    let wrong_validator_list = Keypair::new();

    let mut transaction = Transaction::new_with_payer(
        &[instruction::add_validator_to_pool(
            &id(),
            &stake_pool_accounts.stake_pool.pubkey(),
            &stake_pool_accounts.staker.pubkey(),
            &stake_pool_accounts.deposit_authority,
            &stake_pool_accounts.withdraw_authority,
            &wrong_validator_list.pubkey(),
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
            &stake_pool_accounts.pool_mint.pubkey(),
            &spl_token::id(),
        )
        .unwrap()],
        Some(&payer.pubkey()),
    );
    transaction.sign(&[&payer, &stake_pool_accounts.staker], recent_blockhash);
    let transaction_error = banks_client
        .process_transaction(transaction)
        .await
        .err()
        .unwrap();

    match transaction_error {
        TransportError::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(error_index),
        )) => {
            let program_error = error::StakePoolError::InvalidValidatorStakeList as u32;
            assert_eq!(error_index, program_error);
        }
        _ => panic!("Wrong error occurs while try to add validator stake address with wrong validator stake list account"),
    }
}

#[tokio::test]
async fn test_try_to_add_already_added_validator_stake_account() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    stake_pool_accounts
        .add_validator_to_pool(
            &mut banks_client,
            &payer,
            &recent_blockhash,
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
        )
        .await;

    let latest_blockhash = banks_client.get_recent_blockhash().await.unwrap();

    let transaction_error = stake_pool_accounts
        .add_validator_to_pool(
            &mut banks_client,
            &payer,
            &latest_blockhash,
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
        )
        .await
        .unwrap();

    match transaction_error {
        TransportError::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(error_index),
        )) => {
            let program_error = error::StakePoolError::ValidatorAlreadyAdded as u32;
            assert_eq!(error_index, program_error);
        }
        _ => panic!("Wrong error occurs while try to add already added validator stake account"),
    }
}

#[tokio::test]
async fn test_not_staker_try_to_add_validator_to_pool() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    let malicious = Keypair::new();

    let mut transaction = Transaction::new_with_payer(
        &[instruction::add_validator_to_pool(
            &id(),
            &stake_pool_accounts.stake_pool.pubkey(),
            &malicious.pubkey(),
            &stake_pool_accounts.deposit_authority,
            &stake_pool_accounts.withdraw_authority,
            &stake_pool_accounts.validator_list.pubkey(),
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
            &stake_pool_accounts.pool_mint.pubkey(),
            &spl_token::id(),
        )
        .unwrap()],
        Some(&payer.pubkey()),
    );
    transaction.sign(&[&payer, &malicious], recent_blockhash);
    let transaction_error = banks_client
        .process_transaction(transaction)
        .await
        .err()
        .unwrap();

    match transaction_error {
        TransportError::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(error_index),
        )) => {
            let program_error = error::StakePoolError::WrongStaker as u32;
            assert_eq!(error_index, program_error);
        }
        _ => panic!("Wrong error occurs while malicious try to add validator stake account"),
    }
}

#[tokio::test]
async fn test_not_staker_try_to_add_validator_to_pool_without_signature() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    let accounts = vec![
        AccountMeta::new(stake_pool_accounts.stake_pool.pubkey(), false),
        AccountMeta::new_readonly(stake_pool_accounts.staker.pubkey(), false),
        AccountMeta::new_readonly(stake_pool_accounts.deposit_authority, false),
        AccountMeta::new_readonly(stake_pool_accounts.withdraw_authority, false),
        AccountMeta::new(stake_pool_accounts.validator_list.pubkey(), false),
        AccountMeta::new(user_stake.stake_account, false),
        AccountMeta::new(user_pool_account.pubkey(), false),
        AccountMeta::new(stake_pool_accounts.pool_mint.pubkey(), false),
        AccountMeta::new_readonly(sysvar::clock::id(), false),
        AccountMeta::new_readonly(sysvar::stake_history::id(), false),
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new_readonly(stake_program::id(), false),
    ];
    let instruction = Instruction {
        program_id: id(),
        accounts,
        data: instruction::StakePoolInstruction::AddValidatorToPool
            .try_to_vec()
            .unwrap(),
    };

    let mut transaction = Transaction::new_with_payer(&[instruction], Some(&payer.pubkey()));
    transaction.sign(&[&payer], recent_blockhash);
    let transaction_error = banks_client
        .process_transaction(transaction)
        .await
        .err()
        .unwrap();

    match transaction_error {
        TransportError::TransactionError(TransactionError::InstructionError(
            _,
            InstructionError::Custom(error_index),
        )) => {
            let program_error = error::StakePoolError::SignatureMissing as u32;
            assert_eq!(error_index, program_error);
        }
        _ => panic!("Wrong error occurs while malicious try to add validator stake account without signing transaction"),
    }
}

#[tokio::test]
async fn test_add_validator_to_pool_with_wrong_stake_program_id() {
    let (
        mut banks_client,
        payer,
        recent_blockhash,
        stake_pool_accounts,
        user_stake,
        user_pool_account,
    ) = setup().await;

    let wrong_stake_program = Pubkey::new_unique();

    let accounts = vec![
        AccountMeta::new(stake_pool_accounts.stake_pool.pubkey(), false),
        AccountMeta::new_readonly(stake_pool_accounts.staker.pubkey(), true),
        AccountMeta::new_readonly(stake_pool_accounts.deposit_authority, false),
        AccountMeta::new_readonly(stake_pool_accounts.withdraw_authority, false),
        AccountMeta::new(stake_pool_accounts.validator_list.pubkey(), false),
        AccountMeta::new(user_stake.stake_account, false),
        AccountMeta::new(user_pool_account.pubkey(), false),
        AccountMeta::new(stake_pool_accounts.pool_mint.pubkey(), false),
        AccountMeta::new_readonly(sysvar::clock::id(), false),
        AccountMeta::new_readonly(sysvar::stake_history::id(), false),
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new_readonly(wrong_stake_program, false),
    ];
    let instruction = Instruction {
        program_id: id(),
        accounts,
        data: instruction::StakePoolInstruction::AddValidatorToPool
            .try_to_vec()
            .unwrap(),
    };
    let mut transaction = Transaction::new_with_payer(&[instruction], Some(&payer.pubkey()));
    transaction.sign(&[&payer, &stake_pool_accounts.staker], recent_blockhash);
    let transaction_error = banks_client
        .process_transaction(transaction)
        .await
        .err()
        .unwrap();

    match transaction_error {
        TransportError::TransactionError(TransactionError::InstructionError(_, error)) => {
            assert_eq!(error, InstructionError::IncorrectProgramId);
        }
        _ => panic!(
            "Wrong error occurs while try to add validator stake account with wrong stake program ID"
        ),
    }
}

#[tokio::test]
async fn test_add_too_many_validator_stake_accounts() {
    let (mut banks_client, payer, recent_blockhash) = program_test().start().await;
    let mut stake_pool_accounts = StakePoolAccounts::new();
    stake_pool_accounts.max_validators = 1;
    stake_pool_accounts
        .initialize_stake_pool(&mut banks_client, &payer, &recent_blockhash)
        .await
        .unwrap();

    let user = Keypair::new();

    let user_stake = ValidatorStakeAccount::new_with_target_authority(
        &stake_pool_accounts.deposit_authority,
        &stake_pool_accounts.stake_pool.pubkey(),
    );
    user_stake
        .create_and_delegate(
            &mut banks_client,
            &payer,
            &recent_blockhash,
            &stake_pool_accounts.staker,
        )
        .await;

    // make pool token account
    let user_pool_account = Keypair::new();
    create_token_account(
        &mut banks_client,
        &payer,
        &recent_blockhash,
        &user_pool_account,
        &stake_pool_accounts.pool_mint.pubkey(),
        &user.pubkey(),
    )
    .await
    .unwrap();

    let error = stake_pool_accounts
        .add_validator_to_pool(
            &mut banks_client,
            &payer,
            &recent_blockhash,
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
        )
        .await;
    assert!(error.is_none());

    let user_stake = ValidatorStakeAccount::new_with_target_authority(
        &stake_pool_accounts.deposit_authority,
        &stake_pool_accounts.stake_pool.pubkey(),
    );
    user_stake
        .create_and_delegate(
            &mut banks_client,
            &payer,
            &recent_blockhash,
            &stake_pool_accounts.staker,
        )
        .await;
    let error = stake_pool_accounts
        .add_validator_to_pool(
            &mut banks_client,
            &payer,
            &recent_blockhash,
            &user_stake.stake_account,
            &user_pool_account.pubkey(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        error,
        TransactionError::InstructionError(0, InstructionError::AccountDataTooSmall),
    );
}

#[tokio::test]
async fn test_add_validator_to_pool_to_unupdated_stake_pool() {} // TODO

#[tokio::test]
async fn test_add_validator_to_pool_with_uninitialized_validator_list_account() {} // TODO
