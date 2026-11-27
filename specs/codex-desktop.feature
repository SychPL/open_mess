Feature: Deliver messenger events to an explicit Codex desktop conversation
  The structural contract is .specs/2026-11-26-codex-desktop-triggers.md.
  Native regression checks are in triggers/codex.rs and triggers/outbox.rs.

  Scenario: Admit an event without archiving other people's words
    Given a trigger batch includes message identifiers and untrusted text
    When the Codex destination admits the batch
    Then only message metadata is persisted
    And admission and finalization of remembered message identifiers commit together

  Scenario: Suppress replay even when messages are grouped differently
    Given message one was already admitted for a rule and conversation
    When a new batch contains message one and message two
    Then only message two is admitted for delivery

  Scenario: Do not reroute a batch collected before a configuration edit
    Given a batch is collecting for Codex conversation A
    When the user changes its target to B and then back to A
    Then the original batch is discarded without delivery
    And its message identifiers remain remembered after daemon restart
    And a new matching message starts a fresh batch

  Scenario: Observe an intervening Codex action on a legacy collecting batch
    Given a batch is collecting for a Channel action
    When the user changes that rule to Codex and restores Channel before the next engine observation
    Then the original batch is discarded without delivery
    And a pure Channel to Spawn edit retains the existing legacy behavior

  Scenario Outline: Keep configuration validation consistent with admission
    Given a message matches a rule in configuration A
    When a Codex-related configuration edit occurs <boundary>
    Then the old message is not admitted using configuration B

    Examples:
      | boundary                       |
      | between matching and collection |
      | before final batch admission    |

  Scenario Outline: Recover a durable delivery
    Given a stored delivery is in state <before>
    When the daemon restarts
    Then its state is <after>
    And automatic sending is <send>

    Examples:
      | before            | after             | send      |
      | pending           | pending           | permitted |
      | sending           | uncertain         | forbidden |
      | accepted_by_codex | accepted_by_codex | forbidden |

  Scenario: Keep the hourly send limit across restart
    Given a rule allows one send per hour and already reserved one send
    When the daemon restarts within that hour with another pending event
    Then no second send is reserved
    When the controlled clock advances beyond the original hour
    Then one send can be reserved

  Scenario: Revoke access before delivery
    Given a delivery is waiting for its destination
    When reading is revoked and subsequently restored
    Then the original delivery remains voided
    And it is not automatically sent

  Scenario: Reject authorization at the final protocol boundary
    Given a bounded desktop request is ready to write
    When the final authorization callback refuses it
    Then the desktop receives zero request bytes

  Scenario: Treat a mismatched reply conservatively
    Given a desktop request has been written
    When the response identifier does not match
    Then the result is uncertain
    And there is no automatic resend

  Scenario: Resolve an uncertain delivery without sending again
    Given a delivery has an uncertain result
    When the user dismisses it in the configuration window
    Then its terminal state is voided
    And its replay receipt remains present

  @manual @windows
  Scenario: Deliver while the existing Codex agent is active
    Given the user explicitly selected an existing Codex conversation
    When two synthetic markers are delivered and the second arrives while the agent is active
    Then that conversation acknowledges both markers
    And the test performs no Teams or GitHub action

  Scenario Outline: Keep an unresolved delivery reachable after editing its rule
    Given an uncertain delivery records Codex conversation A
    When the user <edit>
    Then the trusted unresolved-deliveries view still lists that record
    And the view identifies its original conversation A
    And resolving it preserves its replay receipt without sending

    Examples:
      | edit                            |
      | changes the destination to B    |
      | changes the action to Channel   |
      | changes the action to Spawn     |
      | deletes the rule                |

  Scenario: Reach older unresolved deliveries using bounded pages
    Given an uncertain delivery is followed by 100 accepted deliveries for its rule
    And more than 100 unresolved deliveries exist across rules
    When the user follows the unresolved-delivery pages
    Then each page contains at most 100 records
    And all unresolved records are reachable without duplicates
    And the older uncertain delivery can be resolved without sending

  @manual @windows
  Scenario: Recover after an actual desktop restart
    Given a synthetic event remains pending while Codex is closed
    When the user reopens Codex
    Then the worker discovers the current endpoint and delivers the event once
