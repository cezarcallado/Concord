# Security fixes in concord v1.0.1

What v1.0.1 does about the run-2 audit of v1.0.0 (`security-audit-skill\voxhub\run-2`). Each
fix was checked again against the audit record's trace in the current source. Wire changes are
additive (no `PROTOCOL_VERSION` bump): a v1.0.0 app works with a v1.0.1 hub and the other way
round.

| Audit fingerprint | Verdict in the audit | Fix commit | Tests |
|---|---|---|---|
| `voxhub:app:device-token-sent-to-unbound-candidate-address` | needs_validation (high) | `7d14174` (hub identity), `aed6f00` (app) | app `backend::tests::saved_secrets_go_only_to_the_server_they_belong_to`, `backend::tests::the_guard_verdicts`, `servers::tests::the_server_identity_stays_with_the_entry_and_its_token`; hub `tests/api.rs` `the_server_id_is_stable_and_in_health_and_ready`; vox-proto `server_id_tests::*` |
| `voxhub:hub:invite-login-marks-existing-account-server-password-bypass` | needs_validation (medium) | `f212e65` | hub `tests/invites.rs` `an_invite_does_not_take_over_an_existing_name`; app `backend::tests::errors_are_friendly` |
| `voxhub:hub:private-voice-rejoin-window-after-access-loss` | needs_validation (medium) | `11c023a` | hub `livekit_token::tests::join_tokens_expire_within_ninety_seconds`, `tests/private_channels.rs` `old_tokens_keep_being_cut_short_after_losing_access` (and the TTL checks in `tests/api.rs` `voice_join_token`, `tests/voice_guard.rs` `unlisted_participants_are_moderated_and_removed`) |
| `voxhub:hub:mention-id-list-unbounded-db-work` | needs_validation (medium) | `b85c84d` | hub `tests/chat.rs` `huge_mention_lists_are_refused_before_any_work` |
| `voxhub:hub:dm-call-end-reveals-private-voice-join` | confirmed (low) | `af92898` | hub `tests/dms.rs` `leaving_a_call_for_a_hidden_voice_channel_looks_like_a_hang_up` |
| `voxhub:hub:global-sequences-reveal-private-channels` | confirmed (low) | not changed (risk accepted by the owner); documented in `SECURITY.md` (`c323da5`) | none |
| Publication notes (REPORT.md section 4) and `voxhub:publish:third-party-names-and-household-details-in-docs` | confirmed (low) / hardening | `db0ff57`, `bb377d8`, `c323da5` | `hub::host::net` and `vox-capture` tests still pass with the generic values |

## What each fix does

- **Saved secrets and the server identity.** Every hub makes a 128-bit `server_id` once
  (`server_meta`) and returns it in `GET /api/health` and `Ready`. The app keeps it with each
  saved entry (`servers.json`) and with each login token (`settings.json`,
  `device_token_servers`). Before sending a saved server password or login token it reads the
  answering address's `/api/health`: with a known identity, only a matching one gets them; with
  none yet, only the exact address they were saved under (an older hub there works as before).
  Otherwise an invite login goes on with the invite's own token alone, and a saved-entry login
  sends nothing and says "Este endereço não é o mesmo servidor que você salvou", letting the
  user type the credentials again. Identities are pinned only by a confirmed login (matching
  identity, the address the user typed or picked, or an accepted invite). Re-logins check the
  identity first. Removing an entry no longer deletes another entry's token.
- **Invites create accounts only.** An invite on an existing name admits nobody by itself; the
  account's own personal password, remembered device or reset code is needed, and even then the
  invite is not used and the account is not marked as an invite member. Otherwise: "Esse nome já
  existe neste servidor. Escolha outro nome ou peça a senha do servidor."
- **Voice join tokens.** 60 s instead of 10 min (LiveKit adds 60 s of clock leeway). The app
  fetches a new token right before each join, retry and DM call answer; the SDK's own reconnects
  use the token LiveKit refreshes for the session. After any removal the hub checks the rooms
  every second (instead of every 5 s) for as long as an old token could still work.
- **Mentions.** At most 50 ids per message (`400` / WebSocket error), checked before any work;
  the hub looks at no more than that many, each once; the message route reads at most 64 KiB. The
  text cap (4000 characters) is unchanged.
- **DM call end.** The other person gets `Hangup` instead of `JoinedVoice` when they may not see
  the voice channel that was joined.

## Known residuals

- No TLS yet (`voxhub:hub:cleartext-transport-credentials`, confirmed medium): not in this
  release; `SECURITY.md` tells hosts to prefer a VPN.
- A member who lost access to a private voice channel can still rejoin with a token minted just
  before, for about a second at a time, until the token expires (at most about two minutes).
- A personal password the user types while joining with an invite goes to the address the
  invite leads to (only saved data is held back): an invite code can only be trusted as much as
  whoever sent it.
- Message ids stay one global sequence (accepted).
