# Microsoft Teams client protocol

Status: sections 1-8 (2026-10-26), 9 (2026-10-27) and 10 (2026-10-28) come from static analysis: no account was signed in for them, no
request was sent to a Microsoft service, and nothing from the official client's user data
(`%LOCALAPPDATA%\Packages\MSTeams_8wekyb3d8bbwe\`) or any browser profile was read. Facts later
checked on the wire, with the user present, are in "Live results" at the end and override the
static sections where they differ.

Every fact carries one marker:

- **CONFIRMED** - present in the shipped client code or configuration; evidence given.
- **CHANGED** - the spec (section 9) assumed something else; what the code actually does is given.
- **UNCONFIRMED** - static analysis could not settle it; see "Needs live capture".

"Shipped code does X" never proves the server accepts X from a different client. Everything that
open_mess will depend on still needs one redacted live capture before code relies on it (AGENTS.md,
"Protocol facts are verified, not remembered").

## Sources and method

| Artifact | Identity |
| --- | --- |
| MSTeams package (new Teams desktop, "Maglev") | `MSTeams_26260.1717.5229.7049_x64__8wekyb3d8bbwe` |
| Native host `ms-teams.exe` | sha256 `a8034fbcd507d662...` |
| Bundled web client `desktop-assets/` | `@msteams/maglev-included-web-assets` 260813193.14.50801226, 7006 gzip chunks |
| Production config chunk `config-prod-960581273367874e.js` | sha256 `f5f8a4fec2e69b6b...` |

The desktop package ships the complete Teams web client (`desktop-assets/hashed-assets/*.js.gz`).
`desktop-assets/maglev-configs/work.json` sets `rootUri` to `https://teams.microsoft.com/v2`, so this
is the same web client the public `teams.microsoft.com/v2` serves (the CDN copy may be a newer
build). The chunks were decompressed into a scratch directory (stdlib gzip, no code executed) and
analysed there; chunk names below are the file names in that directory.

REA was the primary tool. What it produced, by Evidence id (REA session Evidence, exported bundle):

| Evidence | Operation and input | What it established |
| --- | --- | --- |
| `ev_ab2cb4b6...` | `analyze_javascript_application` on `config-prod` | config inventory; endpoint findings are only `path` properties, string config values are not indexed |
| `ev_3aba636c...` | `analyze_javascript_application` on `precompiled-shared-worker-6249620d...` (18 MB) | failed: analyzer heap exhausted at 4 GiB; graph partial |
| `ev_a894e4bd...` | `analyze_javascript_application` on 31 auth, chat, Trouter, search and file chunks | MSAL literals (`S256`, `retry-after, h429`, `throttling`, `authorization_code`), mock-auth appid `1fec8e78-...`, chat endpoint provider storage keys, GraphQL operations, Trouter `POST` request |
| `ev_c175ad2c...` | `analyze_javascript_application` on chat request builder `37324-1b8abb4bd19306f1.js` | literals `v2/users/ME/endpoints`, `v1/users/ME/conversations`, `v1/threads`, `v1/users/ME/properties?name=`, `v1/users/ME/drafts`, `v1/users/ME/streams/notifications/messages`, `/v1/objects`, `teams/users/me/conversationFolders`, `retry-after` |
| `ev_b69ea909...` | `trace_javascript_semantics` forward from literal `v2/users/ME/endpoints` | the literal is bound to `N` and read by exactly two builders: `At` (PUT endpoint) and `Te` (DELETE endpoint) |
| `ev_825293a6...` | `analyze_javascript_application` on `cmd-trouter-3959f9e7ca693a2a.js` | Trouter JS client version literal `2026.31.01.1`, `MS-CV`, "Skypetoken deprecated response" terminal error |
| `ev_8c44c9ae...` | `analyze_javascript_application` on `msal2-service`, `services-io-users-region`, `services-chat-endpoint-provider`, `services-search-substrate` | `https://teams.microsoft.com/api/authsvc/v1.0/users/region`, endpoint-id store keys `skypexspaces` / `internaldata` / `sync_endpoint` |

REA limits that forced a fallback, recorded so nobody repeats them:

- `search_strings` / `trace_feature` support native targets only; on a JavaScript target REA returns
  `capability_unavailable`. `trace_application_feature` on a string seed only matches graph entities
  (endpoints, modules), not arbitrary literals.
- The REA semantic graph recovers literals only for a part of the modules (coverage `partial`).
- Ghidra (`search_strings` on `ms-teams.exe`, 36 MB) timed out three times at 330 s; opening it from
  `C:\Program Files\WindowsApps` directly fails with access denied, so a copy was used.

So literal locations in the large chunks were found with ripgrep over the decompressed copy and the
surrounding code was read at that offset; for `ms-teams.exe` a byte scan for printable strings was
used. Those facts cite chunk name and the module or function they sit in instead of an Evidence id.

## 1. Sign-in

### 1.1 Client ids

- **CONFIRMED** desktop client id `1fec8e78-bce4-4aaf-ab1b-5451cc387264`. In `ms-teams.exe` it sits in
  the native OneAuth provider: the neighbouring strings are
  `https://login.microsoftonline.com/common/oauth2/nativeclient`, `https://teams.microsoft.com`,
  `auth::OneAuthProvider::Initialize` and the source path
  `src\native_modules\mods\auth\providers\oneauth_provider_win.cpp`. A second occurrence sits next to
  `d3590ed6-52b3-4102-aeff-aad2292ab01c` (Microsoft Office, a FOCI family member) and
  `AcquireCredentialSilently` / `SignInInteractivelyWithCredentials`. In JS the id only appears as the
  `appid` claim of mock tokens (`webview2-service`, `mockauth-service`, REA `ev_a894e4bd...`).
- **CONFIRMED** web client id `5e3ce6c0-2b1f-4285-8d4b-75ee78787346`: `config-prod` `auth.clientId`
  (first key of the config). It is the only client id the JS uses for MSAL; the native binary does not
  contain it.

### 1.2 Who signs in on the desktop

- **CONFIRMED** the desktop (platforms `Maglev*` = 48..51) does not run MSAL.js. `config-prod`
  `auth.enableOneAuth` is true for platforms `[48,50,51,49]`, `enableMsal2` only for `[1454,1415]`
  (1415 = `Web`; platform enum in `async-entry`/`error-*`: `Web=1415, Maglev=48, MaglevT21=49,
  MaglevMacT21=50, MaglevRoomsT21=51`). The web code asks the native host for every token:
  `webview2-service` calls `hostComm.acquireToken({resource, correlationId, accessTokenToRenew,
  claim, silent, userIdentifier, extraQueryParameters, windowId})`.
- **CONFIRMED** the native side uses WAM through OneAuth: `ms-teams.exe` contains (UTF-16)
  `Windows.Security.Authentication.Web.Core` and `WebAuthenticationCoreManager`, and (ASCII)
  `msauth://Microsoft.AAD.BrokerPlugin` and `Microsoft.AAD.BrokerPlugin_cw5n1h2txyewy`.
- **CONFIRMED LIVE (2026-10-26)** a process that is not the MSTeams package can get Teams tokens
  from WAM with `1fec8e78-...`, silently and without any dialog. See "Live results", item 1.

### 1.3 Redirect URIs and embedded WebView

- **CHANGED** the desktop client id's redirect URI is
  `https://login.microsoftonline.com/common/oauth2/nativeclient` (native code, see 1.1). That is the
  classic native-client redirect: an embedded WebView2 can run authorization code + PKCE against it
  and intercept the navigation to that URL to read `code`. No loopback (`http://localhost`) redirect
  appears anywhere for either client id - **UNCONFIRMED** that one is registered; assume not.
- **CONFIRMED** the web client id's redirect URI is `${origin}/${redirectUrlPath}` =
  `https://teams.microsoft.com/v2` (`msal2-service-2ffcc69e...`: `redirectUri:U(t,s)` with
  `U=(n,e)=>`${n.origin}/${e}``, config `redirectUrlPath: "v2"`), authority
  `https://login.microsoftonline.com/common` (`instanceUrl` + `audience` defaults), MSAL Browser
  (`@azure/msal-common v16.4.1`) with `S256` PKCE (REA `ev_a894e4bd...`). This is a SPA
  registration: **UNCONFIRMED** but expected from Entra rules - the code can only be redeemed with an
  `Origin: https://teams.microsoft.com` header and SPA refresh tokens live 24 h at most. Prefer the
  desktop client id for open_mess.
- **CONFIRMED** both web and desktop request CAE: MSAL `clientCapabilities: ["CP1"]` and claims
  `{"access_token":{"xms_cc":{"values":["CP1"]}}}` (`msal2-service`, `getClaimsRequest`); scopes are
  `${resource}/.default` except for a few `service::` audiences.
- **UNCONFIRMED** device code: neither client has a device-code code path (no `devicecode` in
  `ms-teams.exe`; `device_code` in JS is only an MSAL constant). Whether Entra lets `1fec8e78-...`
  use it is unknown.

### 1.4 Token audiences the client requests

From `config-prod` `auth.criticalResources`, `auth.tokenWarmingResources`, `auth.caeEnabledResources`
and the service code (all **CONFIRMED** as requested by the client):

| Use | Audience (resource) | Evidence |
| --- | --- | --- |
| authsvc exchange, middle tier | `https://api.spaces.skype.com` | `authzResource`, `mtResource` |
| chat service (IC3), CAE | `https://ic3.teams.office.com` (non-CAE variant `https://ic3-non-cae.teams.office.com`) | `ic3CaeResource`, `ic3NonCaeResource` |
| chat service aggregator (CSA) | `https://chatsvcagg.teams.microsoft.com` | `csaResource`, `apiConstants.resource` |
| search | `https://outlook.office.com/` (default `substrateTokenUrl`); also warms `https://outlook.office.com/search`, `https://substrate.office.com` | `30208-*.js` `TOKEN_URL`, `tokenWarmingResources` |
| presence | `https://presence.teams.microsoft.com/` and regional `noam`/`emea`/`apac` hosts | `criticalResources` (desktop) |
| Graph | `https://graph.microsoft.com` | `tokenWarmingResources`, `graphEndpointBaseUrl` |
| SharePoint / OneDrive | the site origin, e.g. `https://<tenant>.sharepoint.com` / `-my.sharepoint.com` | `invalidScopeResources` `https://{teamSiteDomain}`, FavoritesManager `authResource: de(personalSiteUrl)` |

## 2. authsvc exchange (Entra token to skype token and region)

- **CHANGED** endpoint: `POST https://teams.microsoft.com/api/authsvc/v1.0/authz`
  (`config-prod` `authzUrl`; `async-entry` constants `AUTHZ_DEFAULT_BASE_URL`). The old host
  `https://authsvc.teams.microsoft.com/v1.0/authz` is kept only as `AUTHZ_DEPRECATED_DEFAULT_BASE_URL`.
  A V2 variant `.../authsvc/v1.0/authzcore` exists behind `enableAuthzV2` (not set in prod config).
  Optional query `?region_gtms_env=<env>`.
- **CONFIRMED** request (`async-entry`, `_getFormatRequest` / header builder `Le`): bearer token for
  `https://api.spaces.skype.com`; no body for a normal user (body only for anonymous/visitor meeting
  joins). Headers: `X-Client-UI-Language`, `claimsChallengeCapable: true`,
  `ms-teams-authz-type: ExplicitLogin | TokenRefresh`, `clientRequestId`, `x-ms-request-id`,
  `x-ms-client-version`, `x-ms-client-type` (defaults to `web`), optional `X-MS-Migration: True`
  (`enableSTMHeaderForAuthZ` is true in prod), optional `ConsistencyLevel`.
- **CONFIRMED** response fields (`mapToDiscoveryResponse` in `async-entry`): the token as
  `tokens.{skypeToken, expiresIn}` or `skypeToken.{skypetoken, expiresIn}` (seconds), `skypeGuestUserToken`, `regionGtms` (map of service
  name to base URL), `region`, `userRegion`, `partition`, `userPartition`, `regionOverride`,
  `regionSettings`, `licenseDetails`, `consumerLicenseDetails`, `ocdiRedirect`, `mri`, `skypeId`. The
  user MRI is `8:<skypeid>` where `skypeid` is a claim of the skype token.
- **CONFIRMED** `regionGtms` keys the client resolves (`regionGtm("<key>")` call sites):
  `chatServiceAfd`, `chatService`, `chatSvcAggAfd`, `middleTier`, `ams`, `amsV2`, `mtImageService`,
  `urlp`, `searchService`, `unifiedPresence`, `presenceUPS`, `teamsPushServiceAfd`,
  `calling_trouterUrl`, `calling_registrarUrl`, `authService`, `graphService` and others.
- **CONFIRMED** a separate `POST https://teams.microsoft.com/api/authsvc/v1.0/users/region`
  (`services-io-users-region`, REA `ev_8c44c9ae...`) returns `regionGtms` for app-platform use.
- **CHANGED** the skype token is being retired ("STM", skype token migration). `config-prod`:
  `skypeTokenMigrationEnabled: true`; `skypeTokenMigrationServiceInfo` marks `mfeService` (the chat
  service), `asyncMediaService`, `urlPreviewService`, `Trouter` and others as `tokenType: "CAE"`;
  `stmSourceList` drops the skype token header for many middle-tier sources. The IC3 gateway helper
  (`async-entry`, `ic3-gateway-token-helper`) picks Skype, AAD or CAE per service and also switches
  when a 401 carries `WWW-Authenticate` with `token_types=`. The skype token is still required by the
  middle tier (`requireSkypetokenForMT: true`, sent as `x-skypetoken`).

## 3. Chat service

### 3.1 Base URL and auth

- **CHANGED** base URL: `regionGtms.chatServiceAfd` when `useChatServiceAfd` (true in prod), else
  `regionGtms.chatService` (`x1e` in the shared worker). AFD hosts are `teams.microsoft.com`,
  `teams.live.com`, `teams.cloud.microsoft` (`chatServiceAfdHostnames`) and the path matches
  `/api/chatsvc/<segment>/`. Requests send `BehaviorOverride: redirectAs404`; a 404 with `Location`
  is a regional redirect, followed up to 10 times.
- **CHANGED** auth: requests carry `authOptions: {source: "mfeService"}`, which STM maps to a CAE
  token for `https://ic3.teams.office.com` (`Authorization: Bearer`). The skype-token form is the
  fallback; header formats in the client are `Authentication: skypetoken=<t>`,
  `x-skypetoken: <t>` and `Authorization: skype_token <t>` (`formatAuthHeader`, `async-entry`).
- **CONFIRMED** conversation id shapes the client recognises: `@thread.v2`, `@thread.skype`,
  `@thread.tacv2`, `@unq.gbl.spaces` (1:1), `19:meeting_...` and `48:notes` (the self-chat, the only
  chat spec 9.5 allows for live write tests).

### 3.2 Operations

Request builders in `37324-1b8abb4bd19306f1.js` (REA `ev_c175ad2c...`), facade method names in
`services-chat-3601245af4214351.js` class `zn`. Paths are relative to the base URL. All **CONFIRMED**.

| Operation | Request |
| --- | --- |
| list conversations | `GET v1/users/ME/conversations?view=msnp24Equivalent&pageSize=<n>&startTime=<ms>` (or `&syncState=<s>`), optional `&targetType=`, `&filterThreadType=` |
| one conversation | `GET v1/users/ME/conversations/<id>?view=msnp24Equivalent` |
| messages (history) | `GET v1/users/ME/conversations/<id>/messages?view=msnp24Equivalent\|supportsMessageProperties&pageSize=<n>&startTime=<ms>[&syncState=][&endTime=]` |
| thread replies (channel reply chain) | same path with conversation id `<channelId>;messageid=<rootId>` (`D.aH(conversationId, replyChainId)`) |
| one message | `GET v1/users/ME/conversations/<id>/messages/<msgId>?view=msnp24Equivalent\|supportsMessageProperties` |
| send | `POST v1/users/ME/conversations/<id>/messages`, JSON body; new id from the `Location` header, `OriginalArrivalTime` in the body |
| edit | `PUT v1/users/ME/conversations/<id>/messages/<msgId>`, JSON body |
| delete | `DELETE v1/users/ME/conversations/<id>/messages/<msgId>?behavior=softDelete` |
| add reaction | `PUT .../messages/<msgId>/properties?name=emotions` body `{"emotions":{"key":"<reaction>","value":<epoch ms>}}` |
| remove reaction | `DELETE .../messages/<msgId>/properties?name=emotions` body `{"emotions":{"key":"<reaction>"}}` |
| thread info / members | `GET v1/threads/<id>?view=msnp24Equivalent`, `GET v1/threads/<id>/members?view=msnp24Equivalent&pageSize=` |
| create thread | `POST v1/threads` body `{members, properties}`; id from `Location` |
| my properties | `GET v1/users/ME/properties` |
| others' read positions (read only) | `GET v1/threads/<id>/consumptionhorizons` |

Paging: responses carry `_metadata.syncState`, `_metadata.backwardLink`, `_metadata.forwardLink`;
the client follows `backwardLink` for older pages (`getChatMessagesFromNetwork` in the shared
worker). **CONFIRMED.** The chat service caps page sizes (`maxPageSizeForConversations`,
`maxPageSizeForConvMessages`; values set by the service facade, not read here).

The conversation list at startup comes from the chat service aggregator (CSA), not from the chat
service (**CHANGED**, new): base `https://teams.microsoft.com/api/csa` (or
`regionGtms.chatSvcAggAfd`, `useHostFromGtmTable: true`), version `/api/v1/`, audience
`https://chatsvcagg.teams.microsoft.com`. Endpoints: `teams/users/me` (startupclean),
`teams/users/me/updates` (startupdelta, `x-ms-synctoken`), `teams/users/me/groupchats`,
`teams/users/me/chats?pageSize=&continuationToken=`. CSA calls carry `x-csendpoint` with the
regional chat service URL. open_mess can use the chat service list alone; CSA is optional.

### 3.3 Message format

**CONFIRMED** (message model in the shared worker, `37324` transforms): a message is JSON with
`content`, `messagetype` (`RichText/Html` for normal messages, `Text`, `Control/...` and
`ThreadActivity/...` for system events), `contenttype` (`text`), `clientmessageid`, `imdisplayname`,
`properties` (`files`, `cards`, `mentions`, `emotions`, `links`, ...), `amsreferences`, `id`,
`version`, `originalarrivaltime`, `from`, `conversationid`. HTML content uses `itemtype` markers such
as `http://schema.skype.com/Mention`, `.../Reply`, `.../Forward`, `.../Emoji`. Inline images reference
AMS objects (`/v1/objects/<id>` in `properties.files[].filePreview.previewUrl`,
`cards[].content.imageData`, `amsreferences`).

### 3.4 Read state - read, never write

Reading (**CONFIRMED**, safe): each conversation carries `properties.consumptionhorizon` formatted
`<originalArrivalTime>;<timeStamp>;<clientMessageId>` (normaliser `Re` in the shared worker; empty
becomes `0;0;0`), an optional "marked as unread" bookmark in `properties.userConsumptionHorizon` or
`properties.consumptionHorizonBookmark` (same format), plus `lastMessageTimeUtc` and `lastMessage`.
A second, account-wide input is the user setting `appConsumptionHorizon` = `{chat: <epoch ms>,
channel: <epoch ms>}` (GraphQL type `AppConsumptionHorizon`, one of the user-preference values),
which "mark all as read" raises instead of touching every conversation
(`enableClearChBookmarkOnMAARChats` is false in prod, so bookmarks stay behind). Which request
returns it is **UNCONFIRMED** (it sits in the user-preference set next to values stored as chat
service user properties, `GET v1/users/ME/properties`).

The client's rule (shared worker, the module exporting `T8`/`il`/`qJ`; functions `L`, `P`, `B`, `U`,
`Z`, `X`), with `app` = `appConsumptionHorizon.chat` for chats or `.channel` for channels:

1. Effective horizon = max(`consumptionhorizon.originalArrivalTime`, `app`). Exception (`ne`/`z`):
   when the conversation's own `originalArrivalTime` is 0 and either the user joined the chat after
   `app` (`threadProperties.lastjoinat > app`) or the channel was created after `app`
   (`threadProperties.createdat > app`), `app` is not applied and the horizon stays 0.
2. The bookmark counts only if `app` is older than the bookmark's `timeStamp` (second field);
   otherwise it is treated as `0;0;0`. A counted bookmark whose `originalArrivalTime` (first field) is
   not 0 and whose `clientMessageId` (third field) is not `"0"` makes the conversation unread - this
   is how "mark as unread" survives a horizon that is already past the last message, and how "mark
   all as read" later cancels it.
3. Otherwise, if the last message was sent by the user (`lastMessage.from` ends with the user's MRI,
   or the `<initiator>` of a call event is the user): read.
4. Otherwise unread when `lastMessageTimeUtc` (fallback `lastMessage.originalarrivaltime`) is greater
   than the effective horizon.

Remaining special cases in the client: call recordings compare `lastContentMessageTime` instead,
channel reply-chain lists use `rclch`/`rclchBookmark`, and a flag-gated variant
(`enableRemoveCustomMsgFromMeBolding`) for system messages. open_mess must implement rules 1-4 and
read `appConsumptionHorizon`; anything it cannot classify is reported as unknown, not as read.
`GET v1/threads/<id>/consumptionhorizons` reads other members' positions.

Writes open_mess must never send (**CONFIRMED** in the builders):

| Write | Request |
| --- | --- |
| mark conversation read | `PUT v1/users/ME/conversations/<id>/properties?name=consumptionhorizon` body `{"consumptionhorizon":"..."}` (`setConversationConsumptionHorizon`) |
| mark as unread / bookmark | same with `name=consumptionHorizonBookmark` |
| reply chain read position | message property `userMetaData` with key `l2ch` (`setReplyChainConsumptionHorizon`); constants `rclch`, `rclchBookmark` |
| mark message read | `PUT .../messages/<msgId>/properties?name=isread` body `{"isread":"true"}` (`markReadStatus`; this is the "markRead" of the spec) |
| batch conversation properties | `setConversationPropertyBatch` (may carry any of the above) |
| mark all as read | raises the user setting `appConsumptionHorizon` (write request **UNCONFIRMED**; never write user settings or `v1/users/ME/properties`) |

**CHANGED**: there is no separate `readReceipt` endpoint. Read receipts other people see are derived
from the consumption horizon (`ThreadActivity/MemberConsumptionHorizonUpdate` control messages), so
blocking the writes above is sufficient. The `PUT v1/users/ME/conversations/<id>/properties?name=`
builder (`Xe`) is generic: the module must allow-list property names instead of block-listing.

## 4. Endpoint and device registration

- **CONFIRMED** chat endpoint (`37324` builders `At`/`Te`, REA trace `ev_b69ea909...`):
  `PUT v2/users/ME/endpoints/<endpointId>` body
  `{"endpointFeatures":"Agent,Presence2015,MessageProperties,CustomUserProperties,NotificationStream,SupportsSkipRosterFromThreads","startingTimeSpan":0,"subscriptions":[{"channelType":"TrouterPush","interestedResources":[...]},{"channelType":"PushNotification","interestedResources":[...]}]}`
  with `interestedResources` = `/v1/users/ME/conversations/ALL/properties`,
  `/v1/users/ME/conversations/ALL/messages`, `/v1/threads/ALL`. The response has
  `subscriptions[0].longPollUrl` and `isActiveUrl`. `DELETE v2/users/ME/endpoints/<id>` removes it.
- **CONFIRMED** the endpoint id is a client-generated UUID v4 (`services-chat-endpoint-provider`:
  `this._endpointId=(0,_.v4)()`), persisted by the official client in its IndexedDB
  `skypexspaces-<user>` / `internaldata` / `sync_endpoint` (REA `ev_8c44c9ae...`). open_mess must
  generate its own UUID once per account, store it in its own storage, and never read that store.
- **CONFIRMED** the same endpoint id is the Trouter `registrationId` and `epid` (section 5;
  `getOverridenConfig(endpointId)` in the shared worker). The CDL worker also registers
  `<registrationId>_GG` ("golden gate").
- **CONFIRMED** a host `deviceId` is compared against the cached endpoint
  (`shouldIgnoreEndpointOnDifferentDevice: true`; mismatch invalidates the cached endpoint) and sent
  as `x-ms-client-device-id` by the location client. open_mess needs its own stable device UUID.
- **CONFIRMED** "active endpoint": `POST <isActiveUrl>` body `{"timeout":135}` and long poll
  `GET <longPollUrl>` with `x-ms-query-params` (90 s timeout) are the polling fallback. **UNCONFIRMED**
  effect of `activeendpoint` on push to the user's phone; open_mess should not call it (see Proposed
  spec changes).
- Presence publishes its own endpoint (`PresenceCreateEndpointHelper`, UPS) and heartbeats; open_mess
  must not publish presence or user activity, or it may change what colleagues see.

## 5. Trouter (live events)

All **CONFIRMED** in `cmd-trouter-3959f9e7ca693a2a.js` (REA `ev_825293a6...`, client version
`2026.31.01.1`) and the CDL worker settings in the shared worker.

1. Token: `Authorization: Bearer <CAE/AAD token>` or `X-Skypetoken: <t>`, plus `X-MS-Migration: True`
   when not on the legacy token API (`setTokenHeader`). `Trouter` is `CAE` in
   `skypeTokenMigrationServiceInfo`; a 401 allocation with header
   `x-trouter-skypetoken-deprecated: 1` is terminal. Audience for the CAE token: **UNCONFIRMED**
   (needs live capture; the IC3 resource is the likely candidate).
2. Allocate: `POST https://go.trouter.teams.microsoft.com/v4/a` (`trouterDefaultEndpoint`, or
   `regionGtms.calling_trouterUrl` with `/v3/c` rewritten to `/v4/a`), `Content-Type: text/plain`,
   query with correlation ids, `epid=<endpointId>` and client info. JSON response (`allocateResult`)
   includes `surl`, `connectparams`, optional `registrarUrl`, reconnect parameters with expiry `se`.
3. Connect: Socket.IO 0.9 style over `wss://.../v4/c` (`X()` swaps `/v4/a` and `/v4/c`), query =
   `connectparams` + `v=v4`, `tc=<client info JSON>`, `timeout=40`, `auth=true`, `epid=<endpointId>`,
   optional `userActivity`. Ping 40 s, pong timeout 5 s, connect timeout 20 s.
   **CONFIRMED** by later analysis (section 10.7): this allocate-then-connect flow is v4a, the
   fallback; the default is v4c, one WebSocket to `/v4/c` with no allocate step. The frames and the
   downstream request envelope are given there.
4. Register: `POST https://teams.microsoft.com/registrar/prod/v2/registrations` (or
   `regionGtms.calling_registrarUrl`) body
   `{"clientDescription":{"appId":"TeamsCDLWebWorker","aesKey":"","languageId":"en-US","platform":"<browser>","templateKey":"TeamsCDLWebWorker_2.6","platformUIVersion":"<ver>","productContext":...},"registrationId":"<endpointId>","nodeId":"","transports":{"TROUTER":[{"context":"","path":"<surl>","ttl":<s>}]}}`.
   The main web client uses `pnhTemplate` `SkypeSpacesWeb_2.6`; registration TTL 3600 s
   (`webRegistrarTtlInSeconds`). Unregister: `DELETE <registrar>/<registrationId>`.
5. Events arrive as Trouter downstream requests (`id`, `method`, `url`, `headers`, `body`) that the
   client must answer with a status (`dispatchDownstreamRequest` / `sendResponse`). Paths the client
   listens on: `/messaging` (chat), `/auth`, `/unifiedPresenceService`, `/tpsUpdate`, `/appsCatalog`,
   `/audienceEvents`, `/b/audienceEvents`, `/b/engage`. Socket.IO events also include
   `trouter.connected`, `trouter.reconnect`, `trouter.message_loss`.
6. `/messaging` bodies are chat notifications with `resourceType` (`NewMessage`, `MessageUpdate`,
   `ConversationUpdate`, `ThreadUpdate`, `EndpointPresence`, `CustomUserProperties`, ...) and
   `resource` (the message or conversation object of section 3.3). Mentions are not a separate event:
   a mention is a `NewMessage` whose `properties.mentions` / content contains the user's MRI.
   Exact envelope (`time`, `id`, compression; `compressedTrouterPayloadPercentage: 100`) is
   **UNCONFIRMED** until a live capture.

## 6. Search

**CONFIRMED** in `services-search-substrate` and its constants module `30208-6132d7c6c1b74e5e.js`:

- `POST https://substrate.office.com/search/api/v2/query` (`SUBSTRATE_BASE_URL` + `QUERY`), or via
  `regionGtms.searchService` + `searchservice/api/v2/query` when `enable3SB2Migration` is on.
- Token audience: `https://outlook.office.com/` (`TOKEN_URL`, overridable by `substrateTokenUrl`).
- Headers: `X-ClientType: MicrosoftTeams`, `X-ClientScenario`, `X-Client-UI-Language`,
  `X-AnchorMailbox` (OID/PUID based, `addSubstrateIdHeaders*`), `clientRequestId`.
- Body: `{"entityRequests":[{"entityType":"...","query":{"queryString":"...","displayQueryString":"..."},"from":0,"size":30,"contentSources":["..."]}],"cvid":"<uuid>","scenario":{"name":"..."}}`;
  results in `EntitySets[0].ResultSets[0].Results[].Source`. The exact `entityType` and
  `contentSources` for chat messages (`Message` / `Teams` by the enums) are **UNCONFIRMED**.

## 7. Attachments

- **CONFIRMED** AMS (inline images, `services-async-media`, shared worker): base
  `regionGtms.amsV2` (`msgEnableAmsV2DomainInMW: true`) or `regionGtms.ams`; config default
  `https://us-api.asm.skype.com/`. Paths `v1/objects/` (create), `v1/objects/<id>/content/<type>`
  (upload: `imgpsh`, `original`, `video`), `v1/objects/<id>/views/<view>` (download: `imgo`,
  `imgpsh_fullsize_anim`, `imgpsh_mobile_save_anim`, `imgt1`, `original`),
  `v1/objects/<id>/views/<view>/status`. Auth is CAE (`asyncMediaService`) or skype token; the client
  also sets an image cookie with `POST v1/skypetokenauth` (form body `skypetoken=<t>`) or
  `v1/aadtokenauth` (`enableAMSAuthServiceWithAADToken: true`). The cookie request retries on 429 and
  network errors: 6 attempts, 1 s doubling, 300 s cap (`AsyncMediaAuthServiceRetryPolicy`).
- **CONFIRMED** files live in SharePoint / OneDrive; the message only references them
  (`properties.files`). Upload uses `${siteUrl}_api/v2.0/drive/root:/<path>:/content` on the user's
  OneDrive (`services-file-upload`).
- **CONFIRMED** host discovery: the personal OneDrive comes from the middle tier
  `GET beta/me/files/availability` (`services-files-personal`), which yields
  `PersonalFileSiteDetails {personalSiteUrl, personalRootFolderUrl, driveId, unifiedDomainSiteUrl}`;
  team sites come from team data (`TeamInfo.teamSiteUrl` / `teamSiteDomain` / `teamSitePath`). The
  SharePoint token audience is the origin of that URL. Middle tier = `regionGtms.middleTier`,
  audience `https://api.spaces.skype.com` plus `x-skypetoken`.

## 8. Throttling, retries and client headers

- **CONFIRMED** chat service retry policy (`CQ`/`KRn` in the shared worker): up to 4 retries;
  429 is always retryable; 5xx and 502/503 (up to 3 endpoint fallbacks); 401 once (after token
  refresh); 404 and 507 never; delay = `max(Retry-After * 1000, min(300000, 1000 * 2^attempt))`;
  a `Retry-After` above 300 s means give up (`yF`). `Retry-After` is read case-insensitively
  (`37324`, REA `ev_c175ad2c...`).
- **CONFIRMED** authsvc retries (`async-entry`): 3 retries, 1 s doubling with jitter, 30 s max delay,
  15 s total, 60 s response cache, 120 s cool-off after failures.
- **CONFIRMED** MSAL honours 429/5xx `Retry-After` in its throttling cache (REA `ev_a894e4bd...`:
  `throttling`, `retry-after, h429`).
- **CONFIRMED** mark-as-read is client-throttled (`markAsReadAPIThrottleRate`) - irrelevant, open_mess
  never calls it.
- **CONFIRMED** headers the services expect: `x-ms-request-priority` (`0` real time, `10` foreground,
  `20` background), `ms-cv` (correlation vector), `x-ms-client-version`, `clientinfo`
  (`os=<os>; osVer=<v>; proc=x86; lcid=<locale>; deviceType=1; country=<cc>; clientName=skypeteams;
  clientVer=<ver>; utcOffset=<+hh:mm>; timezone=<tz>`), `BehaviorOverride: redirectAs404` (chat),
  `X-MS-Migration: True` (STM), `x-ms-request-id`, `x-csendpoint` (CSA), `x-ms-synctoken`,
  `x-ms-continuation`. Exact values the services validate (client version format, client type) are
  **UNCONFIRMED**.

## 9. Meetings and transcripts (stage 2)

Added 2026-10-27 for `.specs/2026-10-28-meetings-transcripts.md`. Same package and decompressed
copy as above. The REA MCP server did not start in that session (connection timeout), so REA ran
through its CLI (`rea analyze-javascript-application`, rea-agents 6.4.0, the same analyzer): Evidence
`ev_85abb977...` over `services-MeetingRecapSwaservice-d88c1b10...`,
`services-meeting-recap-working-set-files-5f7539a1...`, `r_data-resolvers-meeting-intelligence-7c644bf1...`,
`services-sharepoint-share-f3f02827...`, `services-meeting-recap-21224203...` and
`services-meeting-artifacts-bb416b88...`. Literals in the shared worker
(`precompiled-shared-worker-6249620d...`, 18 MB, too large for the analyzer, see above) and in the
calendar chunks were found with ripgrep and read in place. Checks on the wire are in "Live results",
item 9.

### 9.1 Calendar

- **CONFIRMED** the calendar app reads through the middle tier (`regionGtms.middleTier`,
  `services-io-calendar` with the constants in `async-apps-calendar`): V1
  `beta/me/calendarEvents?StartDate=<iso>&EndDate=<iso>`, V2
  `v2.0/me/calendars/default/calendarView?StartDate=<iso>&EndDate=<iso>&shouldDecryptData=<bool>` and a
  paged V2.1 `v2.1/me/calendars/calendarView?startDate=&endDate=&$top=&$count=true&$skip=&$orderby=`.
  `config-prod` sets `enableCalendarV2ApiServiceEnterprise: true` and `enableCalendarV2ApiService:
  false`. Auth is the middle-tier token (`mtResource` `https://api.spaces.skype.com`) plus
  `x-skypetoken` (`requireSkypetokenForMT: true`), with `x-ms-client-type` and
  `x-ms-client-version` (`<platformId>/<buildVersion>`).
- **CONFIRMED** other calendar paths in the same constants: Outlook REST `/v2.0/me/calendarview`
  (`outlookRequestHandler`, desktop calendar sync, audience from `autodiscoverSettings`) and Graph
  `/v1.0/me/calendarView` behind `enableMsGraphForDesktopCalendarSync` (absent from `config-prod`). The
  meeting recap code reads Graph events by iCalendar id: `/v1.0/me/events?$filter=iCalUId+eq+'<ID>'`
  and `/v1.0/me/events/<id>/instances?startDateTime=&endDateTime=&$select=iCalUId,start,end`
  (`r_data-resolvers-meeting-intelligence`, builders `k`, `re`).
- **CONFIRMED** the meeting chat is the thread `19:meeting_<...>@thread.v2`; it is URL-encoded in the
  join link `https://teams.microsoft.com/l/meetup-join/<thread>/0?context=...`. Middle-tier events
  carry the link as `skypeTeamsMeetingUrl` and the meeting data as `skypeTeamsData` /
  `skypeTeamsDataObject` (live, item 9).
- A recurring meeting has one chat thread for all its occurrences; artifacts tell occurrences apart
  by `iCalUid` / `instanceICalUid`, `callId` and start and end times (9.3).

### 9.2 Where transcripts live

- **CONFIRMED** artifact types (`MeetingArtifactType` in the shared worker's GraphQL schema):
  `Transcript`, `TranscriptV2`, `Recording`, `Notes`, `MeetingRecap`, `RecapCrux`, `CallCrux`,
  `AttendanceReport`, `MeetingChat` and others. `TranscriptV2` is a transcript stored in OneDrive or
  SharePoint (ODSP) next to the meeting's recording (the recap code names it `ODSPTranscript`);
  `Transcript` is the older kind kept by the Meeting Artifacts service (9.6).
- **CONFIRMED** the meeting chat announces both: `RichText/Media_CallRecording` (an `URIObject` XML
  document; the client reads `RecordingStatus` `status`/`code`, `ExportResult` `type`, `Id` with
  `type="callId"`, `item` with `type="onedriveForBusinessVideo"` and `RecordingContent`
  `contentTypes`) and `RichText/Media_CallTranscript` (the client reads its `callId`). The recap
  merges four sources: MCPS (9.3), the Substrate working set (9.4), these chat messages and the older
  service (9.6).

### 9.3 A meeting's artifacts: MCPS

- **CONFIRMED** (`services-io-collab-object` in the shared worker, model `listContents`, URL builder
  `Bmr`): `GET <mcps>/contents?threadId=<thread>` with optional `&isSeriesMeeting=`, `&iCalUid=`,
  `&organizerId=`, `&recapCallId=`, `&messageId=`, `&includeTypes=`, `&includePermissionStatus=true`.
  The base is `regionGtms.mcpService` plus the path `contents` (`regionalMCPSEndpointPath`); a base
  containing `artifacts` is rewritten to `mcps`. A list across meetings exists too
  (`listArtifactsByUser`): `<mcps>/user/meetings/contents?top=&userUpn=&startDateTime=&endDateTime=&includeTypes=&skiptoken=&syncstate=`.
- **CONFIRMED** token audience `cmdMeetingArtifactsServiceResource`
  `6bc3b958-689b-49f5-9006-36d165f30e00` (an application id used as the resource, `_cmdMtAuthorize`);
  headers `content-type: application/json;charset=UTF-8`, `x-ms-correlation-id` and, with
  `enableMCPSTelemetryHeadersOverride`, `x-ms-caller-name`, `x-ringoverride`, `x-ms-meeting-type`,
  `x-ms-user-meeting-role`.
- **CONFIRMED** the response is a collection: `resources` of a union type whose `TranscriptV2`
  member has `callId, organizerId, startTime, endTime, threadId, webUrl, driveId, driveItemId, iCalUid,
  transcriptId` (GraphQL type `MeetingArtifactV2TranscriptV2`). Live shape: items 9 and 12 (there
  `startTime` and `endTime` are .NET ticks, not ISO times).

### 9.4 Recordings: Substrate working set

- **CONFIRMED** (`services-meeting-recap-working-set-files`): `GET
  https://substrate.office.com/api/beta/me/WorkingSetFiles?$filter=ItemProperties/Default/MeetingThreadId eq '<thread>'`
  (or `MeetingICalUId` / `MeetingCallId`) `&$orderby=FileCreatedTime desc&$select=SharePointItem,Visualization,ItemProperties/Default/MeetingCallId,ItemProperties/Default/DriveId,ItemProperties/Default/RecordingStartDateTime,ItemProperties/Default/RecordingEndDateTime,ItemProperties/Default/TranscriptJson,ItemProperties/Default/DocumentLink`.
  Headers `Prefer: substrate.flexibleschema,outlook.data-source="Substrate",exchange.behavior="SubstrateFiles"`,
  `scenariotag: Teams_recap_recording`, `client-request-id`. Audience `https://substrate.office.com`
  (`meetingWorkingSetFilesServiceResource`). Only items with `Visualization.Type == "Video"` are used;
  `TranscriptJson` marks a recording with a transcript. It finds recordings, not standalone
  transcripts.

### 9.5 Downloading a transcript from ODSP

- **CONFIRMED** list: `GET <site origin>/_api/v2.1/drives/<driveId>/items/<driveItemId>/media/transcripts`
  (`fetchOdspVideoTranscript` uses `_api/v2.0`); items carry `id, displayName, cTag, languageTag,
  transcriptType (subtitle|caption), isVisible, isAutoGenerated, isDefault, temporaryDownloadUrl,
  source, createdDateTime, lastModifiedDateTime` (zod schema in `928576-*`; the recap transform reads
  `media.transcripts[]` of the item).
- **CONFIRMED** content: `GET .../media/transcripts/<id>/streamContent` returns WebVTT; `?is=1` adds
  voice tags `<v Speaker>` (behind `isTextTrackVttVoiceTagEnabled`); `?format=json` returns the
  player's structured transcript with `entries` and `events` (`pluginOT`); `applymediaedits=true`
  applies recording trims. `temporaryDownloadUrl` is a pre-authorised URL the player uses for text
  tracks. The recap's download menu offers `Vtt`, `Docx` and `Pdf` (`TranscriptDownloadFormat`).
- **CONFIRMED** token audience: the site origin, from the first three URL segments
  (`fetchAuthToken` / `M` in `services-MeetingRecapSwaservice`), e.g. `https://<tenant>-my.sharepoint.com`.
  The origin comes from the artifact's `location` / `webUrl` (9.3).
- **CONFIRMED** live: a download in the user's own organization ("Live results" 12, after item 9
  where the only transcript found belonged to another organization and was refused).

### 9.6 Older transcripts: Meeting Artifacts service

- **CONFIRMED** (`transcriptService` in the shared worker, URL builder `et`): base
  `transcriptServiceEndpoint` `https://api.cortana.ai/MeetingArtifacts/API/v2`, audience
  `transcriptServiceResource` `https://msai.meetingintelligence.ai`, auth source `CMDStable` (CAE in
  `skypeTokenMigrationServiceInfo`). List
  `me/organizer(<oid>)/thread(<thread>)/message(<msgId>)?artifacttypes=Transcript`; content
  `.../call(<callId>)?artifacttypes=Transcript,CallEvents` (JSON `artifacts[].artifactEntries[].jsonContent`);
  download `me/enrichment/organizer(<oid>)/thread(<thread>)/message(<msgId>)/call(<callId>)/artifacttype(Transcript)/format(<Vtt|Docx|Pdf>)?transcriptSource=Ai`
  (file name in `Content-Disposition`). Headers `accept: json`, `x-ms-correlation-id`,
  `x-ms-session-id`, optional `groupId`. Not checked on the wire.

### 9.7 Graph online meetings

- **CONFIRMED** the client never calls Graph `onlineMeetings`: the word occurs, in any letter case,
  in none of the 7033 files of `desktop-assets`. Graph's meeting transcript API is not the client's
  path.
- **CONFIRMED LIVE** the Graph token WAM gives the Teams client id has no `OnlineMeeting*`,
  `OnlineMeetingTranscript*` or `OnlineMeetingArtifact*` scope (item 9), so that API is closed to
  open_mess anyway.

### 9.8 Copilot recap

MCPS also returns `AISummary` and `callCrux` resources, and the recap reads AI notes from
`meetingRecapServiceEndpoint` and `.../api/mcps/prod/meetingcatchup/...`. These are Copilot outputs,
not transcripts; not analysed further (stage 2 leaves summaries to the agent).

## 10. Live captions (stage 3 question)

**Static analysis, not verified live.** Added 2026-10-28 to answer whether open_mess could receive
live captions by joining a meeting itself, as the user's second endpoint without audio or video
(captions exist only for call participants and are never stored; reading them from the official
client is ruled out by AGENTS.md). Same package and decompressed copy as above; ripgrep over the
chunks, no REA Evidence id. Main chunks: `114419-5658b4e2d56443ff.js` (8.8 MB, the JS calling core:
captions, calling bot and data channel services), `calling-electron-c8eaed3906901b6a.js` (the bridge
to the native calling stack), `calling-pluginless-6f37e714be91fccf.js` (the browser WebRTC stack),
`523694-597666624a12ca08.js` (enums), `r_data-client-calling-cb8f40176e83d3b8.js` (GraphQL resolvers),
`services-calling-service-adapters-6bbf4eb6997da139.js`, `calling-locale-en-us-52ee734f3e49b32e.js`.

### 10.1 Calling stack and join

- **CONFIRMED** on the desktop, signaling and media run in the native SlimCore stack outside the web
  view (`services-calling-service-adapters`: `isSlimCoreRunningOutproc(){return!0}`; JS WebRTC only on
  VDI / "pluginless"). The bridge is a global `window.SlimCore` that the host injects with
  `scwv2_bundle.js` (not among the web assets).
- **CONFIRMED** join (`calling-electron`, `SlimCoreElectronCall._joinOrStartSlimCoreCall`):
  `joinSignalingSessionWithMeetingData(<meeting data>, shouldResurrect, {conversationUrl,
  conversationId, conversationType}, mediaPeerType, options)`, options including `threadId`, `messageId`,
  `meetingInfo`, `endpointMetadata`, `participantLegId`, `audioDirection` (default Bidirectional),
  `videoDirection` (default ReceiveFromPeer), `screenshareDirection`, `datachannelDirection` (Disabled
  unless ECS `SkypeCalling.enableDataChannelByDefault`), `muteFlags`, `clientEndpointCapabilities`.
  Directions: 0 Disabled, 1 ReceiveFromPeer, 2 SendToPeer, 3 Inactive, 4 Bidirectional. With the call
  already on another endpoint of the user, `mediaPeerType` becomes `EnterpriseMultiParty`.
- **CONFIRMED** the protocol (seen in the pluginless stack, which speaks it in JS): `JoinConversation`
  and `JoinConversationWithoutCallModality` to `signalingAgentConfig.conversationServiceUrl`, which ECS
  gives at run time (the production host is not in the bundle; `config-prod` has only an override key
  and an on-premises template `https://{appliance_fqdn}:8443/api/v1/conv`). A join with the call
  modality carries an SDP offer; only a user the server marks as a streaming attendee skips it
  (`skipOfferGenerationForStreamingUser`).
- **CONFIRMED** the service calls the client back: every callback link (`/conversation/addModalitySuccess/`,
  `/conversation/contentSharingUpdate/`, ...) is built on `baseMessagingChannelUrl`, the Trouter URL
  (section 5), with the broker as fallback. The local participant is `participants.from: {id, displayName,
  endpointId, participantId, languageId}`. **UNCONFIRMED** whether a registrar registration is required
  too ("Needs live capture" 17).

### 10.2 Transport of the captions

- **CONFIRMED** captions come from a server-side bot in the call, not from a captions service the
  client asks. Starting captions (GraphQL `startClosedCaptions(teamsCallId)`) adds the bot (a `28:` MRI,
  shared with recording and transcription: `addParticipant(getBotDefaultMRI(), ...)` or
  `addCombinedRecorderBot`) if it is not in the call, else sends it a start command. The command is an
  HTTP POST to the bot's `commandUrl`, which the client reads from the bot's roster entry
  (`commandUrl=t.endpointMetadata.commandUrl`), with a skype token and an AAD token: body
  `{timestamp, participantMri, participantLegId, action: "start", mode: "transcription", processingModes:
  [ClosedCaptions], actionParameters, participantSkypeToken}`; other actions `setLanguage`, `addLanguage`,
  `stop`, `userSettingsUpdate`, `setUserToken`.
- **CONFIRMED** the caption text arrives on the call's **data channel**, a media modality:
  `initializeDataChannelForDynamicClosedCaptionsTrack` registers `callingBotService().registerDataCallback(call,
  l, ClosedCaptionsMW, onDataReceived, SourceAndSink)` with `const l=3`, the data id `DATA_ID_TRANSCRIPT=3`
  (`523694`; real-time text is `DATA_ID_RTT=24`). On the desktop the bytes come from a SlimCore data sink
  (`calling-electron`: `_slimcoreInstance.createDataSink(dataId)`, `_dataChannel.registerDataSink(...)`,
  `dataSink.on("data", ...)`, the channel from call property `CallDataChannelObjectId`); in the browser
  stack from a WebRTC `createDataChannel("main-channel")` on the media peer connection.
- **CONFIRMED** each message is JSON: `recognitionResults[]` (speech recognition, source `Ai`),
  `textTracks[]` (`Mt` translation, `Cart`, `Cea708`, `RTT`), `events[]` and others. The UI's
  `ClosedCaptionsEntry` has `id, text, isFinal, userId, userDisplayName, utteranceId, roomId,
  spokenLanguage, trackLanguage, source, timestamp, duration, speakerType`.
- **CONFIRMED** the shipped code has no other path: no captions handler in the Trouter client (`cmd-trouter`), no HTTP or
  WebSocket subscription for ordinary meetings. Town hall and live-stream attendees poll encrypted
  caption chunks over HTTP instead (`services-broadcast-captions`, `{cacheUrl, originUrl, aesKey}` from
  the stream metadata) - not ordinary meetings.

### 10.3 Joining without audio or video

- **CONFIRMED** "Don't use audio" and "Add this device" (companion selection,
  `adjustIntentForCompanionSelection`: outgoing video, outgoing and incoming audio off, the existing
  call id) are a normal media call with mute flags; no mapping to `audioDirection` Disabled was found.
- **CONFIRMED** a signaling-only join exists, "subscribe" (`joinCallWithoutCallModality`, telemetry
  `JOIN_CONVERSATION_WITHOUT_CALL_MODALITY`, role `joinForRosterOnly`, states `CONNECTED_FOR_ROSTER_ONLY`;
  SlimCore `subscribeToSignalingSessionWithMeetingData`): roster and events, no media session, so no
  data channel and no captions.
- **CONFIRMED** the data channel is its own modality: the client itself joins with
  `datachannelDirection` Bidirectional while `audioDirection` is Inactive and `videoDirection` Disabled
  (IP audio/video off, screen-share-only joins), so captions can reach an endpoint that sends and
  receives no audio or video - but only over a negotiated media session (SDP, ICE, DTLS, SCTP).
- **UNCONFIRMED** whether `noMediaTimeoutMs` / `NoMediaFlow` ends a call whose only media is the data
  channel ("Needs live capture" 17).

### 10.4 Language, policy, speakers, side effects

- **CONFIRMED** spoken language: bot command `setLanguage` with `{spokenLanguage}` (per user
  `{userSpokenLanguage}`, subtitles `{subtitleLanguages}`). Anyone may change it while transcription is
  off; while it runs, only the organizer or whoever started transcription (`canChangeMeetingSpokenLanguage`,
  flag `enableSpokenLanguageRestriction`). Every client sees the change through the bot's roster
  `endpointMetadata.spokenLanguage` and shows "Meeting language was changed to ... This will change for
  everyone in the meeting." (no name, no chat message). Default `en-us`, or the bot's `defaultLanguage`;
  automatic detection exists.
- **CONFIRMED** policy: the user's tenant policy `liveCaptionsEnabledType` from the synced user policy
  settings (`meetingPolicySettings`), region gating, and the meeting's
  `meetingDetails.meetingCapability.allowTranscription` from the call's meeting details; bot failures in
  its `endpointMetadata.runtimeErrors`; "Due to policy, you can't copy captions". **UNCONFIRMED** whether
  the organizer has an option for captions alone; none was found ("Needs live capture" 19).
- **CONFIRMED** speakers: each result carries `userId` (an MRI; anonymous ones end in `@local`) and
  `displayName`, `roomId` for room devices; no media-source mapping through the roster is needed.
- **CONFIRMED** CART is the same data-channel stream, a text track with `source: "Cart"`.
- **CONFIRMED** turning captions on adds the bot when it is absent (the client filters `28:` bots out of
  participant lists); the only captions alert is a local "start failed". Transcription, unlike captions,
  announces itself (`TranscriptStartedByOthers`) and is stored. **UNCONFIRMED** whether the service keeps
  anything of captions alone ("Needs live capture" 19).

### 10.5 Visibility of a second endpoint

- **CONFIRMED** the user's own official client notices: `checkForActiveCallOnDifferentEndpoints` ->
  `activeCallOnDifferentEndpointExists` drives "You're in {{title}} on another device. Want to join on
  this one?", "How would you like to join on this device?", "Transfer to this device" and "Add this
  device". This is static analysis only, and the wording is that of the join flow on a device that
  is not in the call. Live ("Live results" 18): with the owner in the meeting in the official client,
  the owner never saw this notice while the probe's leg was in the call.
- **UNCONFIRMED** what other participants see ("Needs live capture" 18): the call model keeps a participant's endpoints as a list
  (`userEndpointCount` = `endpoints.endpointDetails.length`, GraphQL `...endpointDetailsFields`), which
  suggests one roster row with two endpoints; the roster rendering, join toast and sound were not checked.
- **CONFIRMED** blocks on an extra endpoint: end-to-end encrypted meetings take one device
  (`calling_e2ee_meeting_single_device_limit`), immersive events, overflow streaming. Lobby is decided by
  the server per identity (`CallJoinAsLobbyUser`, `autoAdmittedUsers`); no client logic sends a second
  endpoint of an admitted user to the lobby.

### 10.6 Verdict

**Needs media.** Live captions reach a participant only on the data channel of a negotiated media
session; signaling (and the Trouter push) carries the bot's roster entry and commands, not the text,
and the signaling-only "subscribe" join gets no data channel. open_mess would have to do what SlimCore
does: join the conversation service as an endpoint with callback links on its own Trouter connection,
offer SDP with audio and video inactive or disabled and a data channel, run ICE, DTLS and SCTP to the
Teams media relays, and parse the JSON messages of data id 3. That is a WebRTC-class transport, far
beyond the HTTP and WebSocket host imports modules have today (a new host capability, its own spec).

Main risks: the user's own official client may show "You're in this meeting on another device" while
open_mess is in the call (it disturbs the official client, against AGENTS.md, unless the owner accepts
it; live, the owner did not see it while in the meeting, "Live results" 18); others probably see one entry but that is unverified; starting captions when no one has them on
adds the caption bot for everyone; E2EE meetings refuse the second device; a no-media timeout may end
the call; the conversation-service host and media relay details come from ECS at run time and were not
seen. A live check of 10.3 and 10.5 would need the owner's consent and a test meeting.

### 10.7 Join, relays and the data channel (step 0a of the live captions spec)

**Static analysis, not verified live.** Added 2026-10-29 for step 0a of
`.specs/2026-10-29-live-captions.md`: ripgrep over the same chunks, mainly the browser stack
`calling-pluginless-6f37e714be91fccf.js` (which speaks the conversation-service protocol in JS; the
desktop's SlimCore speaks it natively), the calling core `114419-5658b4e2d56443ff.js` and `config-prod`.

**Conversation service and join**

- **CONFIRMED** default service URL `https://api.flightproxy.teams.microsoft.com/api/v2/epconv/`
  (`114419`, `kb.conversationServiceUrl`); ECS namespace `SkypeCalling`, key `conversationServiceUrl`,
  may override it (ECS host `https://config.teams.microsoft.com/config/v1/Teams/`; its query
  parameters are not in the bundle). A client can use the default.
- **CONFIRMED** join = `POST` of JSON to that URL (`startOrJoinCall`, `sendPostRequest`, operation
  `CallSetup`); a 409 re-joins at the `conversationUrl.Location` it returns. Auth: `X-Skypetoken` by
  default; with `clientSupportsGenericTokenAPI` the server's `WWW-Authenticate` picks `skype`, `aad` or
  `cae`, and the AAD forms send `Authorization: Bearer` with the token-migration header. Other headers:
  `X-Microsoft-Skype-Chain-ID` (correlation), `X-Microsoft-Skype-Client`. **UNCONFIRMED** the AAD
  audience (probably `callingTokenLogicalUrl`, default `https://calling.teams.microsoft.com`).
- **CONFIRMED** body (`uw(...)` in the browser stack), keys only:
  `payload.conversationRequest {conversationType, subject, roster {type: "Delta", rosterUpdate: <link>},
  properties, links {conversationEnd, conversationUpdate, localParticipantUpdate, addParticipantSuccess,
  addParticipantFailure, receiveMessage}}`, `groupChat {threadId, messageId}`, `participants.from {id,
  displayName, endpointId, participantId, languageId, meetingRegistrationId, participantPin}`,
  `endpointCapabilities`, `clientEndpointCapabilities`, `endpointMetadata`, `meetingInfo`, `meetingData`,
  `endpointState`, and `callInvitation {links {progress, mediaAnswer, acceptance, redirection, end},
  mediaContent, callModalities, ...}`. The SDP offer goes in `callInvitation.mediaContent` with content
  type `application/sdp-ngc-1.0` (the SDP text in `blob`). The roster-only join (`subscribe`) is the same
  body without `callInvitation`.
- **CONFIRMED** callback links are `<Trouter URL>callAgent/<session id>/<uuid><path>` (`/callAgent/`
  matched on the way in), paths `/call/progress/`, `/call/mediaAnswer/`, `/call/acceptance/`,
  `/call/redirection/`, `/call/end/`, `/conversation/conversationEnd/`, `/conversation/conversationUpdate/`,
  `/conversation/localParticipantUpdate/`, `/conversation/rosterUpdate/` and others; they arrive as
  HTTP requests delivered over Trouter, answered `200`. **UNCONFIRMED** the Trouter envelope and whether
  a registrar registration is needed besides a Trouter connection ("Needs live capture" 5, 7, 17);
  the broker fallback's address was not found.
- **CONFIRMED** answer and roster: `/call/mediaAnswer/` carries `mediaAnswer.mediaContent` (the client
  acknowledges with a `POST` to `mediaAnswer.links.mediaAcknowledgement`); `/call/acceptance/` carries
  `callAcceptance.mediaContent.blob` (final SDP), `acceptedCallModalities` and `links.callLeg`. The
  roster comes as deltas on `/conversation/rosterUpdate/`, each participant with
  `endpoints.endpointDetails[].endpointMetadata`; captions are on when
  `endpointMetadata.processingModes.closedCaptions.state` is `Active` (other states `Inactive`,
  `WaitingForMeetingLive`, `Paused`; `getProcessingModeState`).
- **CONFIRMED** leave = `POST` to `links.leave` from the conversation response, body
  `{participants: {from: {id, displayName, endpointId, participantId, languageId}}, callTransactionEnd:
  {code, subCode, phrase}}`; on page close the client sends the same as a beacon. The meeting's end
  arrives on `/conversation/conversationEnd/` (sub-code `CONVERSATION_END_RECEIVED_FROM_SERVICE` 4100),
  the leg's on `/call/end/`.
- **CONFIRMED** presence without joining: the "on another device" check reads the user's call store,
  `GET https://api.flightproxy.teams.microsoft.com/api/v2/ep/api.userstore.skype.com/api/v1/user/<MRI>/conversations`
  (`config-prod` `callStore.user.url`; `114419` builds `${url}/user/${mri}/conversations`), with
  `X-Skypetoken` or an AAD bearer. **UNCONFIRMED** the item fields that name the meeting.

**Relays**

- **CONFIRMED** TURN hosts from configuration: `default.relay.teams.microsoft.com` (UDP 3478, TCP and
  TLS 443), and `tr.teams.microsoft.com` (`TRAPRelayFqdn`); one configuration variant names the address
  `52.113.193.5`. No wildcard host and no published address ranges of media servers are in the bundle.
- **CONFIRMED** credentials: `GET https://teams.microsoft.com/trap/tokens` with `api-version: 2` and
  `X-Skypetoken` (or a bearer under the newer token API); the answer is `{expires, tokens: [{realm,
  username, password}]}` (realm `rtcmedia`); refreshed early from `earlyRefreshMinutes`. ICE servers are
  built as `turn:<host>:3478?transport=udp`, `turn:<host>:443?transport=tcp`, `turns:<host>:443`.
- **CONFIRMED** ICE: no trickle - the offer is sent after gathering finishes, remote candidates come in
  the answer SDP (no `addIceCandidate`); `iceTransportPolicy` `all` by default, `bundlePolicy`
  `max-bundle`, `rtcpMuxPolicy` `require`.
- **CONFIRMED** offer shape: transceivers start `inactive`; the data channel is
  `createDataChannel("main-channel")` (`m=application ... webrtc-datachannel`). The official client
  keeps at least an inactive audio section; **UNCONFIRMED** whether an offer with only the data section
  is accepted (step 0b).

**Data channel**

- **CONFIRMED** one channel `main-channel`, binary, every message with an 11-byte header, big-endian:
  byte 0 = version 1 in the high nibble and the top 4 bits of the 12-bit header length, byte 1 = the
  rest of the header length (11 + 4 per recipient), byte 2 = start-of-message flag `0x80` and data id
  in bits 0-5, bytes 3-4 = sequence number per data id, byte 5 = fragments that follow, bytes 6-9 =
  source id, byte 10 = recipient count, then 4 bytes per recipient; the payload follows the header,
  fragments are reassembled by sequence number (`depacketize`). Captions (data id 3) are UTF-8 JSON.
- **CONFIRMED** nothing has to be sent on the channel to receive captions: the handler only listens.
- **CONFIRMED** caption entries (`recognitionResults[]`, `textTracks[]`) carry `id`, `text`, `isFinal`,
  `userId`, `displayName`, `spokenLanguage`, `source`, `roomId`, `timestampAudioSent`, `duration`.
  **UNCONFIRMED** the time units.

**Trouter and call signaling, in detail** (added later on 2026-10-29, same method; mainly
`cmd-trouter-3959f9e7ca693a2a.js` and `calling-pluginless-6f37e714be91fccf.js`; settles the static
half of "Needs live capture" 7 and narrows 5 and 17)

- **CONFIRMED** protocol choice (`tokenTypeProtocolSelector`): the default is **v4c**, a WebSocket
  straight to `wss://.../v4/c` with no allocate step. The client falls back to the **v4a** flow of
  section 5 only when `forceV4aProtocol` is set, when `forceV4aProtocolForSkypetoken` is set and the
  token is a skype token (unless `scae` is `"1"`), or after a `v4c-websocket-failure`. The values of
  those flags come from configuration at run time and were not seen.
- **CONFIRMED** v4c query (`buildV4cUrlParams`): `tc=<client info JSON>`,
  `timeout=<pingTimeoutMs / 1000>`, `epid=<endpoint id>`, `ccid`, `dom` (if set), `cor_id`,
  `con_num=<client id>_<attempt>`; no `auth`, no `v`. The server's `trouter.connected` event then
  carries what allocate returns in v4a: `surl`, `connectparams`, `ttl`, `ccid`, `id`,
  `reconnectUrl`, `curlb`.
- **CONFIRMED** (corrected 2026-10-30, REA `ev_548d5ad1...` on `cmd-trouter`, and live, "Live
  results" 13) v4c authenticates inside the socket, not on the upgrade request (a browser WebSocket
  cannot set headers): the socket opens anonymously (state `AnonymousConnecting`), and on `1::` the
  client moves to `WebsocketAuthenticating` and emits the event
  `5:::{"name":"user.authenticate","args":[{"headers":{...},"connectparams":{...}}]}`
  (`sendV4cAuthenticationEvent`). `headers` is `extraConnectionHeaders` - one header,
  `X-Ms-Test-User`, `"False"` for a normal user - plus the token headers of `z(token,
  usingLegacyTokenApi)`: `X-Skypetoken` for a skype token or `Authorization: Bearer` for an AAD or
  CAE token, and `X-MS-Migration: True` unless on the legacy token API. `connectparams` is the cached
  `connectparams` of an earlier connection without `reconnectUrl` and `serviceUrl`, and is left out
  on a first connection. Without this event the server sends `0::` with reason `timeout` 5 s after
  `1::`.
- **CONFIRMED** v4a details: allocate is a `POST` with an empty body and `Content-Type: text/plain`,
  query `cor_id`, `con_num`, `epid`, `tc` (no `auth`, no `timeout`), and the response must be JSON with
  `socketio` (base URL), `surl`, `ccid`, `id`, `ttl`, `connectparams` and optional `curlb`. The
  WebSocket is then a standard Socket.IO 0.9 handshake: `GET <socketio>socket.io/1/?<query>&t=<ms>`
  answers `sid:heartbeat:closeTimeout:transports`, then `wss://<host>/socket.io/1/websocket/<sid>?<query>`,
  where the query is every `connectparams` key plus `v=v4`, `tc`, `timeout`, `auth=true`, `epid`,
  `ccid`, `dom`, `cor_id`, `con_num`.
- **CONFIRMED** frames (Socket.IO 0.9, `type:id[+]:endpoint[:data]`): `1::` connect, `2::` heartbeat
  (echoed back), `3:::<json>` message, `5:<id>[+]::{"name", "args"}` event (`+` asks for an ack),
  `6:::<id>` ack. A downstream request is a `3:::` message with `id` (number), `method`, `url`,
  `headers` (`MS-CV` among them) and `body` (a string). The client answers with a `3:::` message
  `{"id": <same id>, "status": 200, "headers": {...}, "body": "<string>"}`, the headers being
  `MS-CV: <received CV>.<n>`, `trouter-client: {"cd": <ms taken>}` and `trouter-request` /
  `trouter-is-broadcast` echoed when present; an app that does not answer within 5 s gets a 504 from
  the client library itself. Ping is the event `5:<n>+::{"name":"ping"}`, acked by the server with
  `6:::<n>`; `pingTimeoutMs` is 40 s. Events from the server: `trouter.connected`, `trouter.reconnect`,
  `trouter.message_loss`, `audience.subscriptionresult`; from the client: `user.activity` (customary
  after connecting, not required; open_mess never sends it, item 8 of "Proposed spec changes") and
  `trouter.processed_message_loss`.
- **CONFIRMED** compressed callbacks: a downstream request whose header
  `X-Microsoft-Skype-Content-Encoding` (any case) is `gzip` has a base64 body that inflates (gzip or
  zlib, auto-detected) to UTF-8 JSON; the client returns 400 when that fails. The server only
  compresses for an endpoint that set capability bit 128.
- **CONFIRMED** `endpointCapabilities` bits: 1 group video call joining, 2 cloud screen sharing,
  4 hostless group calls, 16 broker incoming, 32 auto-join on conflict, 64 always (server mute),
  128 compressed payload, 512 reinviteless media, 1024 always, 2048 watermark, 4096 audio-only
  watermark, 8192 batched receive message, 16384 batched send message status, 65536 auto-promotion,
  131072 prevent screen capture. The minimum is `64 | 1024` = 1088. `clientEndpointCapabilities` is a
  separate number, 0 by default, sent as `null`.
- **CONFIRMED** join body values for a meeting: `suppressDialout: true`, `scenario: null`
  (`"409RedirectJoin"` after a 409), `conversationType: null`; the four `properties` are booleans from
  configuration (`allowConversationWithoutHost` and the three event-message switches);
  `groupChat.messageId` is the reply-chain id when known, else `"0"`; `capabilities: null`.
  `callModalities` is a subset of `["Audio", "Video", "ScreenSharer", "ScreenViewer"]` - **there is no
  data modality**. `mediaContent` is `{blob, contentType: "application/sdp-ngc-1.0"}`, optionally with
  `requiredFeatures`, `skipIceReinvite` and `clientLocation`. **UNCONFIRMED** the keys of
  `meetingInfo` (only `tenantId`, `organizerId` and `replyChainMessageId` are built explicitly; the rest
  is passed through from the thread's meeting properties), the contents of `endpointMetadata`, the
  `applicationType` value and whether the Trouter URL gets a path suffix before `callAgent/` (it must
  end with `/`).
- **CONFIRMED** the offer as sent: no trickle (sent after gathering), then `toOffer()`, which filters
  candidates, sets port 9 on bundle-only sections and, unless `disableMsSdp` is set, applies the MS-SDP
  transform `toMsSdp`: `msid-semantic: WMS *`, profile `RTP/SAVP`, `a=label` per section
  (`main-audio`, ...), a disabled (port 0) section cut down to type, port, protocol, payload `34`,
  mid and label,
  `a=x-ssrc-range` (non-multistream), `|2^31` on crypto lines, the session fingerprint copied into each
  section, candidate and extension rewrites and `rtcp` set to the m-line port. That `disableMsSdp`
  exists suggests the service also takes plain SDP; **UNCONFIRMED**.
- **CONFIRMED** callbacks: `/call/mediaAnswer/` body `{"mediaAnswer": {sender, mediaContent {blob,
  contentType}, noRingBack}}` (provisional answers the service retries are ignored);
  `/call/acceptance/` body `{"callAcceptance": {links {acknowledgement}, ...}}`, and the client **must**
  `POST {payload: {callAcceptanceAcknowledgement: {links: {mediaRenegotiation, transfer, replacement,
  balanceUpdate, retargetCompletion, ...}}}}` to `links.acknowledgement` (its own callback links). The
  client answers a callback 200 with an empty body; 400 for a missing body, a bad path or a failed
  decode, 404 for an unknown session.
- **CONFIRMED** (added 2026-10-30) joining a call that is already running is not a `POST` to the
  service URL above: `joinGivenConversation(conversationUrl, ...)` calls `startOrJoinCall` with
  `newCall: false` and `conversationServiceUrl` set to the call's own conversation URL, so the join
  goes to that URL, with the body of `uw(...)` (the keys listed above; `groupChat.messageId` is
  the session's `teamsMessageId`, `null` only when there is none - the web client sent `"0"` for a
  "Meet now" meeting, "Live results" 14; no `participants.to`; `endpointMetadata` is
  the session's, an empty object `{}` by default; `endpointState` only when muted or preheating).
  The captured web join ("Live results" 14) shows the full sequence and supersedes this static
  reading where they differ. Only a new call
  (`newCall: true`, body `EV(...)`, which adds `participants.to` and `addModalitySuccess` /
  `addModalityFailure` / `confirmUnmute` links) goes to the service URL. The conversation URL of a
  live call is in the chat's thread property `awareness_conversationLiveState:<n>` (a JSON string,
  key `conversationUrl`) and equals the call store item's `conversationController` ("Live results"
  13). Request headers (`lw`): `X-Microsoft-Skype-Chain-ID`, `X-Microsoft-Skype-Message-ID`,
  `X-Microsoft-Skype-Client` (`Gf(clientInformation)`; the Teams client passes a string, which `Gf`
  keeps as it is: `SkypeSpaces/<client version>/os=<os>; osVer=<version>; deviceType=<type>`, plus
  `; browser=..; browserVer=..` outside the desktop - `signalingAgentConfig.clientInformation` in
  `114419`),
  the token, `X-MS-Migration: True` with the generic token API, and `MS-Teams-Ring` /
  `MS-Teams-Region` / `MS-Teams-Partition` when the local participant has them.
- **CONFIRMED** (corrected 2026-10-30) in the MS-SDP transform (`BO.toMsSdp`) the section cut down to
  type, port, protocol, payload `34`, mid and label is a **disabled** one (port 0), not the data
  section; every section gets the profile `RTP/SAVP` and an `a=label` (`main-audio`, `main-video`,
  else the section's own label).
- **CONFIRMED** join response (any 2xx; 200 or 201 not settled): `conversationController` (the
  conversation URL for later requests), `sequenceNumber`, `links` (`leave`, `addModality`,
  `removeModality`, `mute`, `updateEndpointMetadata`, `getAllParticipants` and others),
  `subscriptionDetails.selfParticipant`, `selfParticipant` (`enableCaptcha`, `enforceConsentToJoin`,
  `preMeetingConsentInfo`) and `roster`. The SDP answer is not in it; it comes on `/call/mediaAnswer/`.

**For the spec:** the steps of 5.2-5.3 hold. Presence can be checked before joining (4.1 takes that
path); the offer should copy the official shape (inactive audio plus the data channel) unless step 0b
shows a data-only offer works; the manifest needs `api.flightproxy.teams.microsoft.com`,
`default.relay.teams.microsoft.com` and `tr.teams.microsoft.com`; `media.peers` has no source in the
client and must come from step 0b (the candidates in the answer) and Microsoft's published ranges.
The Trouter details above add four things for step 0b: the module needs `go.trouter.teams.microsoft.com`
(v4c, one WebSocket) and must answer every downstream request within 5 s; a join needs a call
modality, and since there is no data modality, a data-only join either sends `["Audio"]` with an
inactive audio section or an empty list - which one the service accepts is step 0b; whether a plain
(non-MS) SDP offer is accepted is step 0b; and the client must acknowledge `callAcceptance`. Offline
note from the step 0b probe (two local `webrtc` 0.14 peers, no Teams involved): a data-only offer
reached ICE `connected` and carried framed messages; an offer with an inactive audio transceiver
stayed in ICE `checking`, so an inactive audio section may need work in the host's WebRTC stack.

## Proposed spec changes

Section 9 of `.specs/2026-10-26-open-mess-design.md` should change as follows:

1. 9.1 client ids: keep both, but state their roles. `1fec8e78-...` is the native desktop id with
   redirect `https://login.microsoftonline.com/common/oauth2/nativeclient`; `5e3ce6c0-...` is the
   web SPA id with redirect `https://teams.microsoft.com/v2`. Use the desktop id for open_mess.
2. 9.1 sign-in order: (a) WAM - confirmed live for a third-party process ("Live results", item 1);
   (b) embedded WebView2 with auth code + PKCE against the `nativeclient` redirect is the fallback
   (no loopback redirect); (c) device code has no code path in either client; keep it last and
   flag it as untested.
3. 9.1 tokens: replace "chat API and search tokens" with the audience table of section 1.4, and note
   CAE (`CP1`, claims challenges on 401 with `WWW-Authenticate`) - refresh on a claims challenge, not
   only on expiry.
4. 9.3 authsvc: endpoint `https://teams.microsoft.com/api/authsvc/v1.0/authz` (not the
   `authsvc.teams.microsoft.com` host); the response's `regionGtms` is the service directory for chat
   (`chatServiceAfd`), Trouter, registrar, AMS, middle tier and search.
5. 9.3 skype token: say the client is migrating to AAD/CAE tokens (STM). The chat service, Trouter and
   AMS take a CAE token for `https://ic3.teams.office.com` (to be confirmed live); keep the skype
   token for the middle tier and as fallback, and follow `WWW-Authenticate: token_types=`.
6. 9.3 chat service: base from `regionGtms.chatServiceAfd` with `BehaviorOverride: redirectAs404`
   and 404+Location redirects; list operations of section 3.2; mention CSA as an optional source.
7. 9.2 read receipts: replace the name list with the concrete writes of section 3.4
   (`properties?name=consumptionhorizon`, `consumptionHorizonBookmark`, message `isread`,
   `userMetaData`/`l2ch`, batch property writes) and require an allow-list of writable property names
   (only `emotions` for reactions). There is no separate read-receipt endpoint. The `unread` tool
   (8.5) computes unread state with the client's rule of section 3.4, including the "marked as
   unread" bookmark and the account-wide `appConsumptionHorizon` set by "mark all as read".
8. 9.2 coexistence: own endpoint id = own UUID v4 used for `v2/users/ME/endpoints/<id>`, Trouter
   `epid` and registrar `registrationId`; own device UUID; never call `activeendpoint`, never publish
   presence or Trouter `userActivity`.
9. 9.3 Trouter: v4 allocate (`go.trouter.teams.microsoft.com/v4/a`) + Socket.IO + registrar
   registration; open_mess should register its own PNH app id/template only after confirming which
   values the registrar accepts (the official ones are `TeamsCDLWebWorker` /
   `TeamsCDLWebWorker_2.6`).
10. 9.3 search: `POST https://substrate.office.com/search/api/v2/query` with an
    `https://outlook.office.com/` token and `X-AnchorMailbox`.
11. 9.3 attachments: AMS base from `regionGtms.amsV2`/`ams`; SharePoint host from middle tier
    `beta/me/files/availability` (personal) and team data (team sites).
12. 9.2 rate limits: adopt the client's own retry policy of section 8 (honour `Retry-After`, give up
    above 300 s, background requests marked `x-ms-request-priority: 20`).

## Needs live capture

Static analysis could not settle these; each needs one redacted capture on the user's account, with
the user present (spec 9.5):

1. ~~WAM with `1fec8e78-...` from a non-MSTeams process~~ - done, accepted (see "Live results").
2. Embedded WebView2 auth code + PKCE with `1fec8e78-...` and the `nativeclient` redirect: accepted by
   the tenant's conditional access? Refresh token lifetime?
3. Device code with `1fec8e78-...` (only with explicit consent, spec 9.5).
4. ~~The full authz response shape and real `regionGtms` keys~~ - done (see "Live results", item 2).
5. Which token Trouter allocate and AMS accept from open_mess (the chat service accepts a plain AAD
   token for `https://ic3.teams.office.com` - see "Live results", item 3). Trouter v4c accepts the
   same IC3 token in `user.authenticate` ("Live results" 13); AMS and v4a allocate are still open.
6. Exact values for `x-ms-client-type`, `x-ms-client-version` and `clientinfo` that the services
   accept from a non-official client.
7. Trouter allocate response fields, the downstream request envelope and the `/messaging` payload
   (compression, ack requirements), and which PNH app id/template the registrar accepts for a new
   client. The client side of the envelope, compression and acks is now in section 10.7; what the
   server sends to a non-official client, and the registrar part, still need a capture.
8. Whether `PUT v2/users/ME/endpoints/<id>` alone (without `activeendpoint`) is enough to get
   `/messaging` events, and whether an extra endpoint changes push delivery to the user's phone.
9. Page size limits and `syncState` semantics for conversations and messages, and that the unread
   rule of section 3.4 matches what the official client shows (a chat marked as unread, then "mark
   all as read"), and which request returns `appConsumptionHorizon`.
10. Substrate request body for chat messages (`entityType`, `contentSources`) and the
    `X-AnchorMailbox` format.
11. AMS image download: header token vs cookie, and which view names exist for received images.
12. Whether the public `teams.microsoft.com/v2` build differs from the packaged build analysed here
    (fetching the CDN bundle without signing in and diffing it with REA `compare_bundles` is enough).
13. A transcript download in the user's own organization: `media/transcripts` with a WAM token for
    the site origin, then `streamContent` (WebVTT, `?is=1`) - status, cue count, voice tags, never the
    text (section 9.5). Needs a meeting of the user's own organization with a transcript. Done, see
    "Live results" 12.
14. MCPS `contents` for a recurring meeting (which `iCalUid` it expects: series or occurrence) and for
    a meeting without a transcript; the `paginationInfo` shape. The recurring part and the shape with
    `hasNextPage: false` are in "Live results" 12; a meeting without a transcript and a response with
    `hasNextPage: true` are still to be captured.
15. The `RichText/Media_CallTranscript` content: it starts with `{` but did not parse as JSON
    (perhaps HTML-escaped); its key names.
16. Whether opening a transcript through `streamContent` counts as a view in the recording's
    analytics, visible to its owner. First reading on someone else's recording in "Live results" 12
    (no change right after the download); the next-day reading and the check on the user's own
    recording are still to be done.
17. Live captions (section 10), only if stage 3 goes ahead and with the owner's consent and a test
    meeting: a join with `audioDirection` Inactive, `videoDirection` Disabled and a data channel -
    accepted, kept alive with no audio flowing (`noMediaTimeoutMs`), and needing a registrar
    registration or only a Trouter URL.
18. What other participants and the user's own client show for that second endpoint (roster rows,
    join notice or sound, the "on another device" banner).
19. Whether the organizer can turn captions alone off in the meeting options, and whether the
    service keeps anything of captions that run without transcription (section 10.4).

## Live results

Captured on the user's corporate account with the user present (spec 9.5). Nothing here contains a
token, tenant id, user id or account name.

1. **WAM from a non-MSTeams process - accepted** (2026-10-26). An unpackaged Rust console program
   (`windows` crate 0.62, scratch probe, not in the repo) asked WAM for a token:
   - provider: `FindAccountProviderWithAuthorityAsync("https://login.microsoft.com", "organizations")`;
   - request: `WebTokenRequest(provider, "https://ic3.teams.office.com/.default", "1fec8e78-...")`
     with properties `wam_compat=2.0` and `authority=https://login.microsoftonline.com/organizations`;
   - `GetTokenSilentlyAsync(request)` without an account: `UserInteractionRequired`
     (`0xCAA10001`, `interaction_required`) - expected, no account chosen;
   - `FindAllAccountsWithClientIdAsync(provider, "1fec8e78-...")`: status success, the work
     accounts known to Windows are visible to this client id;
   - `GetTokenSilentlyWithWebAccountAsync(request, account)`: **success, no dialog**. Token claims:
     `aud = https://ic3.teams.office.com`, `appid = 1fec8e78-bce4-4aaf-ab1b-5451cc387264`,
     `scp = Teams.AccessAsUser.All`, lifetime 60-90 minutes, no `xms_cc` (CAE was not requested).

   Consequence for spec 9.1: option 1 (WAM) works for open_mess as a separate program. The sign-in
   flow is: find the provider, list the accounts visible to the client id, let the user pick one in
   the configuration window, then get every token silently with that account; the interactive WAM
   dialog (`IWebAuthenticationCoreManagerInterop::RequestTokenForWindowAsync`) is needed only when
   a silent call returns `UserInteractionRequired`. Requesting CAE (`claims` with `cp1`) is still to
   be tried.

2. **authsvc with a WAM token - accepted** (2026-10-26). `POST
   https://teams.microsoft.com/api/authsvc/v1.0/authz` with `Authorization: Bearer <WAM token for
   https://api.spaces.skype.com>`, headers `claimsChallengeCapable: true`,
   `ms-teams-authz-type: ExplicitLogin`, `X-MS-Migration: True`, `x-ms-client-type: web`, empty
   body: HTTP 200. Response top-level keys: `isMultiGeo`, `licenseDetails`, `ocdiRedirect`,
   `partition`, `region`, `regionGtms`, `regionSettings`, `teamsDataBoundary`, `tokens`. The skype
   token is in `tokens.{skypeToken, expiresIn, tokenType}` (`expiresIn` about 75 minutes). Region
   values look like `emea` / `emea01`. `regionGtms` has 112 keys, including `chatServiceAfd`
   (`https://teams.microsoft.com/api/chatsvc/<region>`), `chatService`
   (`https://<region>.ng.msg.teams.microsoft.com`), `middleTier`
   (`https://teams.microsoft.com/api/mt/<region>`), `ams`, `amsV2`
   (`https://<geo>-prod.asyncgw.teams.microsoft.com`), `calling_trouterUrl`
   (`https://go-<geo>.trouter.teams.microsoft.com/v3/c` - a v3 path, the calling stack's URL; the
   messaging Trouter flow of section 5 is still to be captured), `calling_registrarUrl`,
   `teamsPushServiceAfd`, `search`, `unifiedPresence`. There is no `searchService` key.
3. **Chat service with an AAD token - accepted** (2026-10-26). `GET
   <chatServiceAfd>/v1/users/ME/conversations?view=msnp24Equivalent&pageSize=5&startTime=1` with
   `Authorization: Bearer <WAM token for https://ic3.teams.office.com>` (no CAE claim requested, no
   skype token) and `BehaviorOverride: redirectAs404`: HTTP 200 on the first hop, no regional
   redirect. The skype token is not needed for the chat service. Page keys: `_metadata`
   (`backwardLink`, `forwardLink`, `syncState`, `totalCount`) and `conversations`. Conversation keys:
   `id`, `lastMessage`, `lastUpdatedMessageId`, `lastUpdatedMessageVersion`, `memberProperties`,
   `messages`, `properties`, `targetLink`, `threadProperties`, `type`, `version`.
   `properties` keys: `addedBy`, `addedByTenantId`, `consumptionHorizonBookmark`,
   `consumptionhorizon`, `favorite`, `isemptyconversation`, `isfollowed`, `ispinned`,
   `lastTimeFavorited`, `lastimreceivedtime`; all 5 conversations had `consumptionhorizon`.
   `lastMessage` keys: `clientmessageid`, `composetime`, `content`, `conversationLink`,
   `conversationid`, `from`, `id`, `imdisplayname`, `messagetype`, `originalarrivaltime`,
   `properties`, `rootMessageId`, `s2spartnername`, `sequenceId`, `type`, `version`.
4. **`GET <chatServiceAfd>/v1/users/ME/properties` with the same AAD token - accepted**, HTTP 200.
   Keys include `readReceiptsEnabled`, `selfChatSettings`, `locale`, `favorites`, `userDetails`,
   `personalFileSite`; there is **no `appConsumptionHorizon`** here, so the "mark all as read"
   horizon comes from another request (CSA is the next candidate).

5. **Sending with an AAD token, without registering an endpoint - accepted** (2026-10-26, a real
   message the user asked to send to a colleague, text approved by the user). Steps:
   - `GET https://graph.microsoft.com/v1.0/users?$filter=startswith(displayName,'<prefix>')` with a
     WAM token for `https://graph.microsoft.com/.default` (Teams client id): HTTP 200, so the Teams
     client id can look people up in Graph;
   - the 1:1 chat id is `19:<oidA>_<oidB>@unq.gbl.spaces`; of the two orders only one exists
     (`GET .../conversations/<id>` 200 vs 404), so a client checks both - the order rule is not
     settled from one sample;
   - `POST <chatServiceAfd>/v1/users/ME/conversations/<id>/messages` with
     `Authorization: Bearer <ic3 token>`, `BehaviorOverride: redirectAs404`, body
     `{content: "<p>...</p>", messagetype: "RichText/Html", contenttype: "Text", imdisplayname,
     clientmessageid, properties: {importance, subject, mentions: "[]", files: "[]",
     formatVariant: "TEAMS"}}`: HTTP 201 with `OriginalArrivalTime`.
   No endpoint registration, Trouter connection or skype token was needed to send.

6. **open_mess end to end** (2026-10-27, release build, the owner at the computer):
   - module approval in the window, WAM sign-in from the window (account picker), the account back
     online after a daemon restart with no prompt;
   - `list_conversations` (50 conversations: group, meeting, direct, channel), `read_messages` on a
     1:1 chat in about 1 s, `mentions`, `unread` (every item `unknown`, as designed until the
     account-wide horizon is found);
   - `send` to a 1:1 chat with `confirm`: toast, `Show all`, approved in the dialog, state `done`,
     the message arrived and was answered.
   Found: `read_messages` on `48:notes` timed out in the module (the service answers that page in
   330 ms, see item 7), every `mentions` item carried the same conversation id, and the approval
   showed raw MRIs because the members list has no display names (item 7).
7. **Members and the self-chat** (read-only, 2026-10-27):
   - `GET v1/threads/<id>/members?view=msnp24Equivalent` and `GET v1/threads/<id>`: member keys are
     `capabilities, cid, hidden, id, isFollowing, linkedMri, role, type, userLink` (thread view adds
     `meetingRole, rangeEnd, rangeStart`) - **no display name**. Names must come from elsewhere:
     Graph `users/<oid>` (works with the Teams client id, item 5) or messages' `imdisplayname`.
   - `GET v1/users/ME/conversations/48:notes/messages?...&pageSize=5&startTime=1`: HTTP 200 in
     330 ms; message keys include `fromDisplayNameInToken`, `fromGivenNameInToken`,
     `fromFamilyNameInToken`, `imdisplayname`, `amsreferences`; `_metadata` has `backwardLink`,
     `syncState`, `lastCompleteSegmentStartTime`, `lastCompleteSegmentEndTime`.

8. **The service's own feeds** (read-only, 2026-10-27): the conversation list also returns
   `48:mentions`, `48:notifications` and `48:calllogs` next to the self-chat `48:notes`. They hold
   copies: a `48:mentions` message has the user's mention and `conversationid` `48:mentions`, and
   names the real message in `properties.activity` (`sourceThreadId`, `sourceMessageId`,
   `sourceUserId`, `sourceThreadTopic`, `messagePreview`, `activityType`...). Reading them as
   conversations gave the item 6 bug (every `mentions` item from `48:mentions`); the module now
   leaves them out of every conversation walk (`chat::is_feed`). Messages of real conversations
   always carry their own `conversationid` (20 conversations, 50 messages each, checked). The
   self-chat page itself: one page of 43 messages, 52 KB, about 300 ms, no `backwardLink`; the
   item 6 timeout is still to be reproduced through the daemon.

9. **Meetings and transcripts** (read-only, 2026-10-27, the owner at the computer and agreeing in
   chat; a scratch WAM probe as in item 1, five runs, about 40 GET requests plus one authsvc POST per
   run):
   - WAM gave silent tokens with the Teams client id for: Graph (35 delegated scopes, among them
     `Calendars.Read`, `Calendars.ReadWrite`, `Files.ReadWrite.All`, `Sites.ReadWrite.All`,
     `Mail.ReadWrite`, `Mail.Send`; no `OnlineMeeting*` scope); `https://substrate.office.com` (17
     scopes); the MCPS resource `6bc3b958-689b-49f5-9006-36d165f30e00` (`User.Read
     user_impersonation`); the SharePoint site origin `<origin>/.default` (audience the SharePoint
     application id `00000003-0000-0ff1-ce00-000000000000`, scopes `Container.Selected MyFiles.Write
     Sites.FullControl.All Sites.Manage.All User.ReadWrite.All`; with `claims` asking for `CP1` the
     token carries `xms_cc`); `https://msai.meetingintelligence.ai` (a token the probe could not decode
     as a JWT). `<origin>/AllSites.Read` and the bare origin as a scope failed with `0xCAA20002`.
   - authsvc `regionGtms` has `mcpService` (`https://teams.microsoft.com/api/mcps/<geo>`, a geography
     like `eu`, not the chat region) and `cmdArtifactsService`; no calendar key.
   - Middle tier `GET <middleTier>/v2.0/me/calendars/default/calendarView?StartDate=&EndDate=&shouldDecryptData=false`
     with the `https://api.spaces.skype.com` token and `x-skypetoken`: HTTP 200, a JSON object with
     `type` and `value`, 130 events for 30 days in one response (290 KB, no paging). Event keys:
     `appointmentSequenceNumber, bodyPreview, categories, cleanGlobalObjectId, conflictingMeetings,
     doNotForward, endTime, eventTimeZone, eventType, iCalUID, inviteAllMembers, isOnlineMeeting,
     isResponseRequested, lastModifiedTime, location, myResponseType, objectId, onlineMeetingConfLink,
     organizerName, schedulingServiceUpdateUrl, seriesMasterId, showAs, skypeTeamsData,
     skypeTeamsDataObject, skypeTeamsMeetingUrl, startTime, subject, utcOffset`.
   - Graph `GET /v1.0/me/calendarView` (30 days, `$top=100`, `$select`): HTTP 200 with
     `@odata.nextLink`; `onlineMeeting` keys `conferenceId, joinUrl, tollNumber`; every one of 94
     Teams meetings had the `19:meeting_...` thread in `joinUrl`. Not used by the spec (no new
     audience needed for the middle tier).
   - Meeting chats through the chat service (section 3): `RichText/Media_CallRecording` content is an
     `URIObject` with `RecordingStatus[status,code]`, `ExportResult[type]`, `MeetingOrganizerId`,
     `MeetingOrganizerTenantId`, `ICalUid`, `MeetingICalUid`, `RecordingStorage`, `Identifiers/Id[type,value]`
     and `item[type,uri,driveId,driveItemId]`; `RichText/Media_CallTranscript` content (618 bytes)
     starts with `{` but did not parse as JSON; its `properties` hold only `s2spartnername`. Of the
     10 latest meeting chats one had transcript messages.
   - MCPS `GET <mcpService>/contents?threadId=<thread>&iCalUid=<id>` with the MCPS token: HTTP 200,
     top keys `changeKey, countOfFilteredContentsByChatHistory, deletedResources, invalidResources,
     paginationInfo, resources`; resource types `AISummary`, `TranscriptV2`, `callCrux`, `notes`.
     A resource has `addedBy, additionDateTime, id, location, mcoResourceId, metadata, subType, type`;
     a `TranscriptV2`'s `metadata` has `appTitle, callId, containerId, correlationId, driveId,
     driveItemId, endTime, fileId, fileTitle, fileType, iCalUid, instanceICalUid, isMultilingual,
     meetingJoinUrl, messageId, organizerId, organizerTenantId, ownerId, ownerTenantId,
     sharePointOnlineId, sharingLinkPermissionId, startTime, threadId, thumbnailUrl, transcriptId,
     userId, webUrl`; its `location` origin is the owner's SharePoint.
   - Substrate `WorkingSetFiles` by thread: HTTP 200, 0 items for that meeting.
   - That transcript belonged to a meeting organized by another organization (`ownerTenantId` not
     the user's; the owner confirmed). Its SharePoint refused every token of the user's own tenant:
     HTTP 401 `unauthenticated` for the item and for `media/transcripts`, with and without `CP1`;
     Graph `drives/<id>/items/<id>`: 404 `itemNotFound`; WAM for the owner's tenant authority:
     provider error `0xCAA20003`. A download in the user's own organization was captured later
     (item 12).
   - In the first run a tag-name filter of the probe printed part of a recording's file name to the
     terminal; the filter was fixed and nothing of it was written to a file or this repository.

10. **Step 0 of the stage 2 spec, calendar part** (read-only, 2026-10-28, the owner at the computer;
    a scratch WAM probe printing only status codes, key names, counts, enum values, character classes
    and lengths):
    - Middle tier `calendarView` for a dense window of 32 days (31 back, 1 ahead): HTTP 200 in about
      0.9 s, 316 KB, top keys only `type` and `value`, 140 events in one response - **no paging** for
      the window of `teams_list_meetings` (at most 31 days).
    - Keys over all events, with how many events had them: `appointmentSequenceNumber` 106,
      `bodyPreview`, `categories`, `cleanGlobalObjectId`, `conflictingMeetings`, `doNotForward`,
      `endTime`, `eventTimeZone`, `iCalUID`, `inviteAllMembers`, `isOnlineMeeting`, `lastModifiedTime`,
      `location`, `myResponseType`, `objectId`, `organizerName`, `showAs`, `startTime`, `subject` 140,
      `eventType` 129, `seriesMasterId` 129, `isResponseRequested` 137, `utcOffset` 139,
      `schedulingServiceUpdateUrl`, `skypeTeamsData`, `skypeTeamsDataObject`, `skypeTeamsMeetingUrl` 131,
      `onlineMeetingConferenceId`, `onlineMeetingTollNumber` 107, `isOrganizer` 28,
      `onlineMeetingConfLink` 24, `onlineMeetingTollFreeNumbers` 13, `isAppointment` 3,
      `sensitivityLabelId` 2, and once each `clientOptimisticEventId`, `isAllDayEvent`,
      `meetingAgenda`, `meetingTemplateId`, `shortOnlineMeetingJoinUrl`. Keys with a default value are
      left out (`eventType` is missing on single events, `isOrganizer` on events the user does not
      organize).
    - `eventType`: `Occurrence` 61, `Exception` 68, missing 11 (single events). `myResponseType`:
      `Organizer` 68, `NotResponded` 70, `Accepted` 2 (no declined invitation in the window).
      `showAs`: `Tentative` 110, `Busy` 29, `Oof` 1.
    - No `isCancelled`, `sensitivity` or `isPrivate` key in any event: no cancelled or private
      meeting in the window, or the middle tier never sends them; `sensitivityLabelId` is a Purview
      label, not the private flag. The module skips an event whose `isCancelled` is true or whose
      `sensitivity` is `Private`, should they come.
    - `startTime` and `endTime`: always `YYYY-MM-DDTHH:MM:SS+hh:mm`, with the offset (280 of 280).
      `eventTimeZone` is a shortened Windows zone name (`CentralEuropeanSt` 139, `Utc` 1) and
      `utcOffset` a number (null once); the offset in the time is enough to get UTC.
    - `skypeTeamsMeetingUrl` on 131 events, all `.../l/meetup-join/<thread>/...` with a
      `19:meeting_...@thread.v2` thread of 69 characters whose middle part is only `[A-Za-z0-9]`.
      The longest `objectId` or `iCalUID` was 168 characters.
    - `GET <chatServiceAfd>/v1/users/ME/properties`: `personalFileSite` is a JSON document in a
      string, keys `availability` (number), `lastModifiedTime`, `personalRootFolderUrl` and
      `personalSiteUrl`; `personalSiteUrl` is `https://<t>-my.sharepoint.com/personal/...` with a
      lowercase `<t>` (letters only for this tenant), no port.

11. **Call store and relay credentials** (read-only, 2026-10-29, the owner in a real meeting at the time
    and agreeing in chat to reads only - no join, no Trouter; scratch probe, counts and key names only):
    - `GET .../api.userstore.skype.com/api/v1/user/<MRI>/conversations` with `X-Skypetoken`: HTTP 200, a
      JSON array. The first read, right after the owner joined, gave 0 items; the next ones gave 1, with
      and without `?nocache=<ms>` and with or without `ms-teams-region` / `ms-teams-partition`. Item
      keys `additionalData` (key `participantList`), `conversationController` (an https URL),
      `correlationId`, `participants` (an array of 2 strings), `userJoinTime`. No field holds the
      meeting's `19:meeting_...` thread, so the call store says that the user is in a call, not in
      which meeting: the live captions spec can check "in a call" before joining, and "in this
      meeting" only from the roster after joining (spec 4.1, second path). The store may lag a join.
    - `GET https://teams.microsoft.com/trap/tokens` with `X-Skypetoken` and `api-version: 2`: HTTP 200,
      top keys `expires` (a number) and `tokens` (one entry, its `realm` a quoted string as 10.7 says).

12. **A transcript download in the owner's organization** (read-only, 2026-10-29, the owner at the
    computer; scratch WAM probe, counts, key names, character classes and lengths only, no transcript
    text, names or ids). This is **not the D3 gate of step 0**: the recording is a colleague's, and the
    owner decided explicitly in chat that this one meeting's transcript may be read before the D3
    check, accepting that a view, if the download counts as one, lands in the colleague's analytics.
    The decision covers only this meeting; the D3 gate (on the owner's own recording, both counters,
    the player's count and a control) still has to pass before `teams_get_transcript` ships.
    The meeting: one occurrence (`eventType` `Exception`) of a recurring meeting of the owner's own
    organization, organized by a colleague, transcribed, about an hour long. The probe made exactly the
    requests of spec 4.2 steps 1-6, once:
    - MCPS `contents`: HTTP 200 with `threadId` and the calendar event's `iCalUid`, and with
      `threadId` alone. Top keys `changeKey`, `countOfFilteredContentsByChatHistory`,
      `deletedResources`, `invalidResources`, `paginationInfo`, `resources`. `paginationInfo` is
      `{"hasNextPage": false}` in both; no continuation field was seen, so `hasNextPage: true` has
      no known shape yet. With `iCalUid` the resources were 1 `TranscriptV2`, 1 `AISummary`, 1
      `notes`; with the thread alone 3, 3 and 2 (the whole series). So the calendar event's
      `iCalUID` picks the occurrence, although the resource's `metadata.iCalUid` differs from it
      and from `metadata.instanceICalUid` ("Needs live capture" 14, recurring part).
    - The `TranscriptV2` resource: keys `addedBy`, `additionDateTime`, `id`, `location`,
      `mcoResourceId`, `metadata`, `subType`, `type`; `metadata` keys `appTitle`, `callId`,
      `containerId`, `correlationId`, `driveId`, `driveItemId`, `endTime`, `fileId`, `fileTitle`,
      `fileType`, `iCalUid`, `instanceICalUid`, `isMultilingual`, `meetingJoinUrl`, `messageId`,
      `organizerId`, `organizerTenantId`, `ownerId`, `ownerTenantId`, `sharePointOnlineId`,
      `sharingLinkPermissionId`, `startTime`, `threadId`, `thumbnailUrl`, `transcriptId`, `userId`,
      `webUrl`. **`metadata.startTime` and `endTime` are not ISO times but strings of 18 digits,
      .NET ticks** (100 ns since 0001-01-01 UTC): read so, they were 0 and 64 minutes after the
      calendar event's start. `ownerTenantId` was the account's tenant; `organizerTenantId` was an
      empty string; `ownerId` was neither the owner nor the organizer (who it names - whoever
      started the recording or the transcription - is NOT confirmed, so spec 4.2 does not rely on
      it). `id` 60 characters `[-0-9A-Za-z]`, `driveId` 66 `[!0-9A-Za-z]`, `driveItemId`
      34 `[0-9A-Z]`, `transcriptId` 36 `[-0-9a-z]` - all inside the classes of spec 4.2 step 4.
      `location` lay on the organization's OneDrive origin (`https://<t>-my.sharepoint.com`), and the
      WAM token for that origin was granted.
    - `GET <origin>/_api/v2.1/drives/<d>/items/<i>/media/transcripts`: HTTP 200, `value` with 1
      entry, visible and default, keys `cTag`, `displayName`, `id`, `isAutoGenerated`, `isDefault`,
      `isVisible`, `languageTag` (`pl-PL`), `size`, `source`, `temporaryDownloadUrl`,
      `transcriptType` (`subtitle`). **Its `id` equals `metadata.transcriptId`** ("Needs live capture"
      13). `cTag` 27 characters `[0-9:=A-Za-z]`, the same before and after the download.
    - `.../media/transcripts/<id>/streamContent?is=1`: HTTP 200, `text/vtt`, about 157 KB for the
      hour. The body starts with a byte order mark before `WEBVTT`. 1021 cues, every timing
      `HH:MM:SS.mmm --> HH:MM:SS.mmm` with no cue settings, every cue one `<v Name>...</v>` and no
      other tag. The module's own `parse_vtt` and `page` (copied verbatim into the probe) parsed it:
      1021 cues, all with a speaker, 9 distinct speakers, no empty or cut text, at most 84 characters
      per cue, starts in order; `page` with the default `max_bytes` 131072 gave 2 pages (131014 and
      5445 bytes of JSON), with 4096 a first page of 34 cues.
    - D3, view counters on someone else's recording, so only what any viewer's player reads:
      `GET .../items/<i>/analytics/allTime` gave HTTP 200, keys `@odata.context`,
      `@oneDrive.displayNamesOfFileViewers`, `access` (`actionCount`, `actorCount`,
      `timeSpentInSeconds`), `aggregationInterval`, `citation`, `endDateTime`, `incompleteData`,
      `isTrending`, `startDateTime`. `access.actionCount` and `actorCount` were 0 before the
      download, right after it and about two minutes later. **Not a D3 result yet**: the counters may
      lag, so they are read again a day later, and the check on the owner's own recording (spec step 0)
      still stands. `@oneDrive.displayNamesOfFileViewers` shows that a counted view would carry the
      viewer's name, which is why D3 matters.

13. **Step 0b of the live captions spec, second day** (2026-10-30, the owner at the computer and
    agreeing in chat to each run; scratch probe outside the repo, own random endpoint id, no audio
    sent or received, printing only status codes, key names, counts and redacted error bodies):
    - Trouter v4c with `user.authenticate` (section 10.7): HTTP 101, `1::`, the event sent at once
      with an `Authorization: Bearer` IC3 token (`https://ic3.teams.office.com`), `X-MS-Migration`
      and `X-Ms-Test-User: False`; `trouter.connected` came 49-61 ms later, in five runs. Its keys:
      `ccid`, `connectparams` (`issuer`, `scae`, `se`, `sig`, `sp`, `sr`, `st`), `curlb`, `dur`,
      `healthUrl`, `id`, `reconnectUrl`, `registrarUrl`, `socketio`, `surl` (https), `ttl` (about
      a week, in seconds), `url`. A connection with no meeting lived the full 61 s it was given,
      with pings acked and 5 `trouter.message_loss` events and no downstream request. So an IC3 AAD
      token is enough for Trouter ("Needs live capture" 5, Trouter part) and the 5 s `timeout` of
      the first day was the missing `user.authenticate`.
    - Call store: the items now have the keys `conversationController`, `correlationId`,
      `groupContext` (`messageId`, `threadId`), `meetingInfo` (`organizerId`, `tenantId`) and
      `userJoinTime`, and `groupContext.threadId` is the chat thread of the call - unlike item 11,
      where no field held the thread (that was a scheduled meeting; these were a "Meet now" meeting
      and a group chat call). The owner was in 2, then 3 calls at once.
    - The thread property `awareness_conversationLiveState:<n>` was missing on the owner's own
      "Meet now" chat although he was still in that call, so it is not a reliable "call is live"
      flag. On a group chat with a running call it was a JSON string with `callStartTime`,
      `conversationId`, `conversationType`, `conversationUrl` (https), `expiration`,
      `groupCallInitiator`, `isHostless`, `meetingInfo` (`isBroadcast`, `organizerId`,
      `tenantId`) and `status`; its `conversationUrl` equalled the call store's
      `conversationController`, of the form
      `https://api.flightproxy.teams.microsoft.com/api/v2/ep/<conversation host>/conv/<id>?i=<13 digits>&e=<18 digits>`.
    - `GET` of that conversation URL with `X-Skypetoken`: HTTP 200, about 41 KB, top keys
      `activeModalities`, `compositionServiceDetails`, `conversationController`,
      `conversationStartTime`, `creatorClientVersion`, `layoutDetails`, `links`, `meetingDetails`,
      `meetingInfo`, `meetingStates`, `originalGroupCallInitiator`, `region`, `roster`,
      `sequenceNumber`, `state`, `streamingSetupFailureDebugInfo`, `subject`, `tenantId`; `links`
      include `addModality`, `addParticipantAndModality`, `conversationHttpTransport`,
      `getAllParticipants`, `leave`, `removeModality`, `updateEndpointMetadata`; the response's own
      `conversationController` equals the URL read, and `addModality`, `addParticipantAndModality`
      (`.../add`) and `leave` are that URL with a path segment inserted before the query, while
      `conversationHttpTransport` is a plain `http://` URL ending in `/enc`.
    - Join attempts (each refused, so no call leg was created; the owner saw nothing in his client
      and the other participants reported nothing). In the owner's "Meet now" meeting, alone: `POST`
      to the service URL `.../api/v2/epconv/` gave HTTP 400 with the body `{}`, with a data-only
      offer and `callModalities: []` and with an inactive audio section and `["Audio"]` - the wrong
      URL for a running call (section 10.7). In a group chat call of the owner's friends (with the
      owner's explicit OK in chat for that call; the probe only counts and never reads content):
      `POST` to the conversation URL gave HTTP 503 in 240-320 ms with `operationFailure`
      `{reason: "unknown", code: 503, subCode: 70051, phrase: "Proxy Operation Failure.",
      resultCategories: ["UnexpectedServerError"]}`, for a plain data-only offer, a data-only offer
      with the MS-SDP changes of 10.7 and an MS-SDP offer with an inactive audio section, and no
      Trouter callback arrived while the request was open. Later the same day, in the same call and
      with the owner's OK, each of these changed nothing (still 503, subCode 70051, in 200-340 ms): a
      registrar registration of the probe's endpoint first (`POST <registrarUrl>` from
      `trouter.connected`, `registrar/prod/V2/registrations`, body of section 5 with
      `TeamsCDLWebWorker` / `TeamsCDLWebWorker_2.6`, the surl as the only transport and TTL 900 s,
      `Authorization: Bearer` IC3 token: HTTP 202 with an empty body; `DELETE
      <registrarUrl>/<registrationId>` afterwards: HTTP 202), `endpointMetadata: {}`, the
      `X-Microsoft-Skype-Client` value in the client's own format, the `MS-Teams-Region` and
      `MS-Teams-Partition` headers, a roster-only join (no `callInvitation`, so no SDP at all), and
      even the body `{}`, while a `GET` of the same URL with the same token works. The 503 is an
      `UnexpectedServerError`, which an empty body also produces, so it does not by itself show
      whether the body matters. Relay candidates were also missing from these offers (TURN
      allocation on `default.relay.teams.microsoft.com` gave none), which matters only once a join
      is accepted. Item 14 has the captured web join and the probe runs that followed it.
    - Still open for step 0b: whether a data-only offer is accepted, data id 3 messages, and
      whether the call lives without audio.

14. **A captured join of the Teams web client** (2026-10-30, the owner's choice and OK in chat). Method:
    a separate Edge instance with an empty temporary profile and a loopback-only DevTools port (not
    the official desktop client, whose data and process were not touched); the owner signed in there
    himself before any recording began; then a small DevTools listener of ours recorded only the
    call-signaling requests while he joined his own "Meet now" meeting from that window, and printed
    methods, URL shapes, header names, the values of a few non-secret headers, JSON key trees,
    enum-like values and the SDP structure (addresses, ports, keys, fingerprints and ids cut; a slip
    that printed roster map keys was scrubbed from the scratch log at once). REA's own
    `capture_browser_scenario` refused to attach to the existing tab (`target_not_allowed`), so REA
    was used for the static half only. The web client runs at `https://teams.cloud.microsoft`.
    Every request below carried `content-type: application/json`, `ms-teams-region`,
    `ms-teams-partition`, `ms-teams-ring: general`, one `x-microsoft-skype-chain-id` for the whole
    call, a fresh `x-microsoft-skype-message-id`, `x-ms-migration: True`, `x-skypetoken` and
    `x-microsoft-skype-client:
    SkypeSpaces/1415/<build>/os=windows; osVer=NT 10.0; deviceType=computer; browser=edgeChromium; browserVer=<v>/TsCallingVersion=<v>/Ovb=<hash>`,
    over HTTP/2. The sequence:
    1. Registrar registrations from the page and from a worker (`POST
       https://teams.microsoft.com/registrar/prod/V2/registrations`).
    2. `POST https://api-<region>.flightproxy.teams.microsoft.com/api/v2/epconv` - a regional host,
       no trailing slash - with **no `callInvitation`**: `conversationRequest` (`subject: null`,
       `roster`, `properties` with `enableGroupCallEventMessages: true`, `links` whose callbacks
       point at `http://broker.invalid/callAgent/...` because Trouter was not ready yet),
       `groupContext: null`, `groupChat: null`, `participants.from` (`languageId` `pl-pl`),
       `capabilities: null`, `endpointCapabilities: 73463`, `clientEndpointCapabilities: 114259690`,
       `endpointMetadata.holographicCapabilities`, `meetingInfo: null`, `meetingData` (`meetingCode`
       16 digits, `passcode` - the `p` of the join link, `meetingUrl`), `meetingPreferences`
       (`shouldResurrect: "Resurrect"`), `endpointState` (`endpointStateSequenceNumber: 0`,
       `endpointProperties.additionalEndpointProperties`), `debugContent`. Answer: HTTP 200 with
       `conversationController`, `links`, `meetingDetails`, `roster`, `state`
       (`conversationType: "scheduledMeeting"` for a "Meet now" meeting) and a `location` header.
       This is how a meeting join link (`/meet/<code>?p=<passcode>`, which holds no thread) leads to
       the running call.
    3. `POST <conversation URL>/notificationLinks` with `participants.from`, `roster` and the full
       set of Trouter callback links (`https://<trouter host>/v4/f/<id>/callAgent/<session>/<tag>/conversation/...`):
       HTTP 200. It moves the callbacks from the broker to Trouter.
    4. `POST <conversation URL>` (the `conversationController` from step 2, which equals the live
       call's URL) - the join: the body of `EV(...)` (`participants.to: []`, `contentSharing: null`,
       `conversationRequest.links` with `addModalitySuccess` / `addModalityFailure` /
       `confirmUnmute`), `groupChat` (`threadId`, `messageId: "0"`), `meetingInfo` (`organizerId`,
       `tenantId`), `meetingData` (`meetingCode`, an 8-character `passcode` the service returned),
       `meetingPreferences`, `endpointState` (also `preheatProperties`), `participantPropertyBag`, and
       `callInvitation` with `callModalities: ["Audio", "ScreenViewer"]`, `replaces`, `transferor`,
       `clientTransferContext`, `customContext` and `emergencyContent` all `null`, the five call
       links, `clientContentForMediaController` (`controlVideoStreaming`, `csrcInfo` links),
       `pstnContent` (`emergencyCallCountry`, `platformName`, `publicApiCall`), `locationContent`,
       `networkContent`, `areaContent` `null`, and `mediaContent` with `contentType:
       "application/sdp-ngc-1.0"`, `blob`, `clientLocation` (2 letters), `mediaDescriptions`
       (`descriptions[]` of `mid` and `direction`, `requestId`), `applyChannelParameters` and
       `mediaLegId` (32 characters). HTTP 200.
    5. `POST https://api.flightproxy.teams.microsoft.com/api/v2/ep/<cc host>/cc/v1/active/...` with
       `callParticipantUpdate.links` (the client's call callbacks) and
       `clientContentForMediaController`: HTTP 202. Then `updateEndpointState` (POST) and
       `updateEndpointMetadata` (PUT) on the conversation.
    - **The offer** (`blob`): 13 sections in one BUNDLE, all `RTP/SAVP` with `a=x-signaling-fb`,
      `a=x-ssrc-range`, `a=setup:actpass`, `a=ice-options:trickle`, `a=rtcp-mux` and `a=label`:
      `main-audio` (sendrecv), ten `main-video` (sendrecv and recvonly), `applicationsharing-video`,
      and **the data channel as `m=x-data <port> RTP/SAVP 127 126` with `a=x-data-protocol:sctp`,
      `a=label:data` and `a=sctp-port:5000`** - not `m=application ... webrtc-datachannel` (that is
      what `toMsSdp` makes of it; section 10.7 did not show it). The offer held a single host
      candidate.
    - A separate broker channel is long-polled throughout: `GET
      .../api/v2/ep/<broker host>/api/v1/subscribe/<id>/0?i=<n>`, HTTP 202 with `message`,
      `nextSubscribeUrl` and `expirationTimeInSec`.
    - The probe after the capture (same meeting, the owner in it with captions on, his OK for each
      run): step 2 copied from the web client returned HTTP 200 and the live call's URL - but only
      with the web client's `endpointCapabilities` (without the broker and reinviteless bits:
      72935); with 1088 the service answered **201 and created a new conversation** for the meeting,
      which ended at once (`/conversation/conversationEnd/` arrived during the next request), and an
      empty meeting gives 201 too. Step 4 with the web client's body keys, one chain id per call,
      an `m=x-data` offer (data only, and with an inactive audio section), and a roster-only join
      after step 2, all still gave 503 subCode 70051 in about 300 ms. What is left between the web
      client and the probe is the transport and the browser headers (HTTP/2, `User-Agent`,
      `Origin`), the remaining body values (`clientEndpointCapabilities`, `languageId`,
      `endpointMetadata`, `pstnContent`, `mediaDescriptions`, `participantPropertyBag`) and the rest
      of the SDP (13 sections, trickle). Not tested: a probe that copies a browser's `User-Agent` and
      `Origin` was stopped before it ran - open_mess must not pass itself off as a browser.
    - Later runs the same day (same meeting, owner's OK): over HTTP/2 (negotiated by ALPN) with an
      honest `User-Agent` of the probe, step 2 again gave 200 and **step 3 `notificationLinks` gave
      HTTP 200**, and a `/conversation/rosterUpdate/` callback (keys `participantCounts`,
      `participants`, `sequenceNumber`, `type`) arrived over Trouter - so POSTs to the conversation
      URL do work and the service knows the probe's endpoint. The join of step 4 still gave 503
      70051 with each of: `subject` taken from step 2's answer, `pstnContent` as the client builds
      it (`emergencyCallCountry: ""`, `platformName` = the client header, `publicApiCall: false`),
      the web client's `endpointCapabilities` 73463 and `clientEndpointCapabilities` 114259690 with
      `mediaDescriptions` (`mid`, `direction` per section, `requestId`), an offer whose m= and c=
      lines carry the default host candidate's address and port with that one candidate,
      `a=ice-options:trickle`, `b=CT:4000`, `a=x-ssrc-range`, `a=rtcp`, `a=x-signaling-fb` and
      `a=max-message-size:262144`, and with the audio section `inactive` or (as a diagnostic, no
      track, nothing sent or received) `sendonly`. A second capture, with the owner choosing not to
      use audio in the web pre-join screen, showed the same offer: the web client always offers
      audio `sendrecv`, ten video sections and `m=x-data`, and mutes afterwards. After the probe's
      registrar registration, chat notifications arrived on the Trouter path `/messaging` (keys only
      were looked at); the registration was deleted after each run (HTTP 202).
    - **Solved later the same day, see item 15** (the probe wrapped the join body in `payload`).
      What the service missed in step 4 was first thought to be one of: the remaining body values
      (`languageId`, `endpointMetadata.holographicCapabilities`, `endpointState.endpointProperties`,
      `participantPropertyBag`, `applyChannelParameters`, the `confirmUnmute` link), the offer's
      other sections (audio `sendrecv` plus video, which open_mess may not use) and the browser
      request headers.

15. **The probe joins the call** (2026-10-30, the owner alone in his own "Meet now" meeting with
    live captions on, his OK in chat for each run; a file-only review by Codex pointed at the first
    item below):
    - **The wire body has no `payload` level.** The client's request builder `lw` sends
      `JSON.stringify(data.payload)`, and the builders (`uw`, `EV`, `_V` for leave, the acceptance
      acknowledgement) return `{payload: {...}}`, so what goes on the wire is the inner object - as
      the capture of item 14 shows (`conversationRequest`, `participants`, `callInvitation` at the
      root). Section 10.7 lists those keys under `payload.`; read them as root keys. The probe had
      wrapped its join, leave and acknowledgement in `payload`, which is what the 503 70051 of items
      13 and 14 answered (an empty `{}` too). Unwrapped: the join returned **HTTP 200** with the
      conversation (about 40 KB), `leave` HTTP 204 and the acceptance acknowledgement HTTP 202.
    - Offer with only `m=x-data` and `callModalities: []`: join 200, then `/call/end/` at once.
    - Offer with an audio section `inactive` plus `m=x-data`, `callModalities: ["Audio"]`: join 200,
      `/call/acceptance/` came with the answer: one BUNDLE (`0`), `m=audio 3478 RTP/SAVP 111 9 0 8`
      `inactive`, `a=setup:passive`, `a=x-source-streamid`, four relay candidates on port 3478 in
      52.112.0.0/16 (two UDP with an `MTURNID <id>` extension, two `tcp-pass`), and **`m=x-data 0
      RTP/SAVP 127 126` - the data section rejected**; `acceptedCallModalities: []`; the acceptance
      `mediaContent` keys `blob`, `contentType`, `escalationOccurring`, `fromMixer`, `mediaLegId`,
      `newOffer`, `skipIceReinvite`. ICE never connected.
    - The same with the audio transceiver **`sendonly` and no track** (nothing is sent; the probe
      refuses any answer whose audio section would send audio to it, failing closed when no
      direction is given) and `endpointState.state.isMuted: true`: the answer's audio was `recvonly`,
      ICE **connected** to the relay candidates, and the data section was again `m=x-data 0`. Then
      the service ended the leg about 2-3 s later: `/call/end/` with `callEnd` code 410, subCode
      301067, phrase `DTLSHandshakeFailed`, reason `clientError`, result categories `JoinFailure`,
      `Other`. webrtc-rs logged `Failed to start manager dtls: ring::error::Unspecified`.
    - **The media processor's DTLS certificate is ECDSA P-384 and signs with SHA-256.** The `dtls`
      crate 0.13 that webrtc-rs 0.14 uses verifies ECDSA with SHA-256 only on P-256, so the
      handshake failed in `verify_signature`; `allow_insecure_verification_algorithm` does not cover
      it. With a local patch to the probe's copy of the crate that picks the curve from the
      certificate's key (`ECDSA_P384_SHA256_ASN1`), the peer connection reached **connected** and the
      leg stayed up for the 90 s of the run, the service sending only RTCP; the probe then left
      (HTTP 204). Step 1 of the live captions spec needs a WebRTC stack that verifies P-384 with
      SHA-256 (the fix upstreamed or carried).
    - **The web client's own answer accepts the data section.** A further capture (same method as
      item 14) showed that the web join's HTTP 200 response itself carries `callAcceptance` (with
      `acceptedCallModalities: []`, `links` for `acknowledgement`, `callLeg`, `mediaRenegotiation`,
      `startOutgoingNegotiation`, `applyChannelParameters` and more, and `mediaContent` with
      `skipIceReinvite: true`), and its answer SDP has `m=x-data <port> RTP/SAVP 127 126` with
      `a=x-source-streamid`, `a=setup:passive`, `a=x-ssrc-range`, `a=x-data-protocol:sctp`,
      `a=label:data`, `a=rtcp-mux` - accepted - next to audio and eleven video sections. The client
      acknowledged it with `POST .../cc/v1/callAcceptance/.../acknowledge`.
    - For the probe the data section stayed at port 0 with each of: a video section `sendonly` with
      no track added (answered `recvonly`, the leg stayed up), the web client's
      `clientEndpointCapabilities` 114259690, the reinviteless bit 512 without `mediaDescriptions`,
      and `/TsCallingVersion=<v>` in the client string. Bit 512 with the probe's own
      `mediaDescriptions` (one per section) ended the leg at once: `callEnd` 410, subCode 301999,
      `InvalidFinalAnswerMediaDescriptions`; the client builds that list only from `sendrecv`
      sections and adds the data section only with `subTypes` (`getMediaDescriptions`). In the
      client, data `subTypes` are set later (`setSubTypes`) and only for real-time text in PSTN
      calls, not for captions.
    - **The data channel is started by a renegotiation in the client** (static, `calling-pluginless`
      and `SlimCoreWebview2.dll`): `startDataChannel` enables modality 3 and calls
      `updateMediaModalities`; the native code has `DataChannelStart` and "Data channel operation
      offer" (the desktop joins with `datachannelDirection` Disabled by default). The renegotiation
      is `POST <callAcceptance.links.mediaRenegotiation>` with `{"mediaNegotiation":
      {"callModalities", "sender" {id, displayName, endpointId, participantId, languageId}, "links"
      {"mediaAnswer", "rejection" (path /call/rejection/)}, "mediaContent"}}` (`qH`); the answer
      comes on `/call/mediaAnswer/` and is acknowledged with an empty `POST` to
      `mediaAnswer.links.mediaAcknowledgement`. Closed captions register a handler for data id 3 on
      the calling bot's data channel (`registerDataCallback(call, 3, ClosedCaptionsMW, ...,
      SourceAndSink)`); the handler decodes `recognitionResults` / `textTracks` JSON.
    - Live, in a meeting of the owner's with ten other people (his explicit OK in chat for that
      meeting; the probe only counts): join with audio `sendonly` only (no data section) - 200,
      acceptance, ICE and DTLS connected; three seconds later the renegotiation adding `m=x-data`
      - HTTP 202, `/call/mediaAnswer/` with audio `recvonly` and **`m=x-data 0` again**,
      acknowledgement HTTP 202; the leg stayed up for 120 s, then left (204). That meeting's bot had
      `closedCaptions` and `realTimeTranscript` `Active`. Asked afterwards, the other participants
      had seen nothing of the probe's endpoint (the owner's report) - a first answer to "Needs live
      capture" 18 and decision O2 of the live captions spec. The owner then ran, himself, the same
      probe with the web client's build number in `X-Microsoft-Skype-Client` (a run the session's
      permission classifier had stopped): the same result, `m=x-data 0` in the renegotiation answer -
      the client version is not what the service decides on.
    - Two diagnostics the owner decided on also left the data section at port 0: audio `sendrecv`
      with no track (the service then sends audio; webrtc-rs dropped it undecoded, as there was no
      track handler) and the renegotiation above. The cause was elsewhere - item 16.

16. **Live captions received** (2026-10-30, the owner's meeting with nine or ten other people,
    captions on, his OK in chat for a series of runs; the probe counts, prints key names only and
    never reads, prints or stores caption text, names or ids):
    - **The cause of the rejected data section**: the client's MS-SDP converter (`Kz.modify` in
      `calling-pluginless`) gives the `m=x-data ... 127 126` section the lines
      `a=rtpmap:127 x-data/90000`, `a=rtpmap:126 rtx/90000` and `a=fmtp:126 apt=127`; the probe
      did not, and the service answered the section with port 0. (The capture tool of item 14 left
      `rtpmap` and `fmtp` lines out of its print, which hid them.) For the answer the client's
      `Xz` turns the section back into `m=application <port> UDP/DTLS/SCTP webrtc-datachannel`,
      drops its `rtpmap`, `fmtp`, `rtcp` and x- lines and adds `a=sctp-port` (the answer has none).
    - **The working offer**: one BUNDLE of `m=audio` `sendonly` with no track (nothing is sent; the
      service answers `recvonly`, so it sends no audio) and `m=x-data <port> RTP/SAVP 127 126`
      with the three lines above, `a=x-data-protocol:sctp`, `a=x-ssrc-range:<n>-<n>`, `a=rtcp-mux`,
      `a=setup:actpass`, `a=sendrecv`, `a=sctp-port:5000`, `a=max-message-size:262144`,
      `a=label:data`, one host candidate and `a=ice-options:trickle`; `callModalities: ["Audio"]`,
      one `mediaLegId` for the call. The data section goes in the first offer; no renegotiation is
      needed.
    - **The answer** (in `/call/acceptance/`): audio `recvonly` on port 3478 and **`m=x-data 3478
      RTP/SAVP 127 126`** with `a=label:data` and `a=x-data-protocol:sctp`, four relay candidates
      in 52.112.0.0/16; ICE and DTLS (P-384, item 15) connected and the SCTP data channel
      `main-channel` opened once.
    - **Captions arrived with nothing sent on the channel**: in 120 s 301 messages, all on data id
      3, framed as section 10.7 says (0 broken frames); 299 recognition results, 24 of them
      final. A second run of 30 s: 55 messages, 54 results, 11 final. Message top keys
      `events`, `recognitionResults`, `version`; result keys `confidence`, `displayName`,
      `duration`, `id`, `isFinal`, `model`, `source`, `speechRecognitionServiceLatency`,
      `spokenLanguage`, `temporaryStorageOnly`, `text`, `timestampAudioSent`, `trackLanguage`,
      `userId`. No `textTracks` in these runs.
    - So step 0b of the live captions spec is answered, but for one offer only: audio `sendonly`
      with no track plus the data section is accepted. The data-only offer and the offer with
      audio `inactive` (item 15) were tried only before the three lines above were added, so
      whether either of them works now is still open (a retest, step 0c of the spec). The leg
      lives without audio flowing (two minutes, ended by the probe's own `leave`, HTTP 204), the
      caption messages are on data id 3, and the other participants saw nothing (item 15 - asked
      after runs whose data channel did not open; not asked again after these runs). The
      join that received captions went through the `epconv` request with `meetingData` from a
      meeting link; the join at the call store's `conversationController` (item 15, HTTP 200)
      has not yet carried the data section. The steps the module
      needs are those of items 14-16: registrar registration, Trouter v4c with
      `user.authenticate`, the regional `epconv` request with `meetingData` (or the live call's
      URL), `notificationLinks`, the join without a `payload` envelope, the acceptance
      acknowledgement, MS-SDP both ways, and a DTLS stack that verifies P-384 with SHA-256.

17. **What the roster shows for the probe's leg** (2026-10-30, the owner's own "Meet now" meeting with
    an anonymous guest he joined himself as an observer and, in the second run, a colleague he
    invited for the test; his OK in chat for both runs; the probe printed only key names, counts and
    enum values of the owner's own roster entry):
    - Run A, the owner in the meeting: the probe's join (HTTP 200) added a second endpoint to the
      owner's entry (`endpoints` 1 to 2, `state: active`, `isMuted: true`, `role: admin`) - one
      roster entry with two endpoints (what the observer saw at that moment was not asked). The
      owner then hung up in the official client: his entry stayed
      `active` with one endpoint (the probe's), and the observer still saw him in the meeting. The
      entry went only when the probe left (`leave`, HTTP 204). Captions kept coming (36 messages, 7
      final, all on data id 3).
    - Run B, the owner not in the meeting (the probe's own presence check turned off for this run):
      the `epconv` request with `meetingData` from the link and the join both gave HTTP 200, with no
      lobby (the owner organizes the meeting); the owner's entry appeared `active` with one endpoint,
      and the observer saw the owner join. The data channel opened and captions came (110
      messages, 18 final; a result key `rawText` appeared next to those of item 16).
    - So a leg is never hidden: whenever any endpoint of the account is in the call, the account is
      in the roster as present. A leg without the user is not an invisible listener but a stand-in
      that shows the user as present - which is why the live captions spec joins only while the user
      is in the meeting and leaves when the user's last other endpoint goes. Anyone holding the
      user's tokens could do the same; that is an account compromise, not a gap in the service.
    - `timestampAudioSent` is a number that is not a Unix time (s, ms, us), .NET ticks or a
      FILETIME within 10 minutes of the local clock, so probably relative to something;
      `duration` was of the order 1e6-1e7 (in 100 ns units that would be 0.1-10 s; unconfirmed).

18. **Step 0c of the live captions spec** (2026-11-02, a scheduled team meeting the owner organizes, from
    the owner's calendar, the colleagues in it told about the tests, captions on in the owner's
    official client, the owner's OK in chat for the runs; the probe printed only codes, key names, counts and enum values). All joins
    went the module's way: the conversation URL from the call store item whose
    `groupContext.threadId` is the meeting chat, no `epconv`, no `notificationLinks`, and the probe
    left Trouter unread while the join `POST` was open, answering its callbacks only afterwards (as
    the module will, its frames arriving between calls):
    - **Scheduled meeting**: the call store had one item, keys `conversationController`,
      `correlationId`, `groupContext` (`messageId`, `threadId`), `meetingInfo` (`organizerId`,
      `tenantId`), `userJoinTime`; `groupContext.threadId` was the meeting chat's thread, and
      `conversationController` was the same URL as the `conversationUrl` of the chat's
      `awareness_conversationLiveState` (an https URL on `api.flightproxy.teams.microsoft.com`). A
      `GET` of that URL returns the conversation with the `roster` and `links` (`leave`,
      `notificationLinks` among them). Item 11 found no `groupContext` on a scheduled meeting on
      2026-10-29; what differed then is not known.
    - **Join at the call store's URL, callbacks answered late**: HTTP 200 in 0.35-1.4 s; no callback
      was answered while the `POST` was open, and the acceptance with the SDP answer came after it.
      Offer audio `sendonly` with no track plus the data section: the data channel opened, 183
      messages in 90 s, 19 final results, 0 broken frames. The service does not wait for the
      callbacks' answers.
    - **Data-only offer** (one `m=x-data` section, `callModalities: []`): accepted, answer
      `m=x-data 3480 RTP/SAVP` with four relay candidates in 52.112.0.0/16, ICE and DTLS connected,
      126 messages in 60 s, 15 final. So no audio section is needed.
    - **Audio `inactive` plus the data section**: answered with audio `inactive` and the data section,
      but ICE failed (one try, not repeated, since the data-only offer works); the roster delta then
      showed the owner's entry with one endpoint.
    - **Callbacks**: one `/call/acceptance/` and one `/conversation/rosterUpdate/` per run, and 5
      `trouter.message_loss` events in every run (cause unknown; test B of item 17 had 1).
    - **Trouter reconnection** (the same day, no meeting): after a clean close, a new socket 3 s later
      with the same `epid` and the cached `connectparams` (without `reconnectUrl` and `serviceUrl`) in
      `user.authenticate` got `trouter.connected` with the same `surl`, `id`, `ccid` and
      `reconnectUrl`, both at `reconnectUrl` (a `wss://` URL on `*.teams.microsoft.com:443`, path
      `v4/c`, no query) and at the default `go.trouter` URL.
    - **The user's endpoints in the roster**: the entry's `endpoints` is an object keyed by endpoint
      id. The probe's endpoint showed `languageId` `en-US`, `isMuted: true`, one media stream,
      `endpointCapabilities` 67 and no `endpointMetadata`; the official client's showed the client's
      language, four media streams and `endpointMetadata.transcriptionPrefs.closedCaptions: true`.
    - **Others**: the owner asked the colleagues after the runs; nobody saw anything. The owner,
      in the meeting in the official client, never saw the "on another device" notice of
      section 10.5 - that notice belongs to a device that is not in the call.
    - **A leg that vanishes without `leave`** (the probe exited after captions came, with no `leave`,
      no DTLS or SCTP close and no unregister): the leg stayed. Reads of the conversation every 15 s
      showed the owner's entry with two endpoints, `state: active`, for 10 min 20 s; the probe then
      sent the skipped `leave` from a new process as that endpoint (its `endpointId` and
      `participantId` from the roster entry, picked as the one endpoint with the probe's own
      `clientVersion`, `en-US`, muted and no transcription preferences) - HTTP 204, and the next read
      showed one endpoint. So a leg with no media flowing does not end within 10 minutes when its
      client vanishes - whether it ends when the registration's 900 s TTL runs out, or only with
      the meeting, is not known - and a `leave` sent as that endpoint from another process ends it.

All checks in items 1-4 and 7-12 were read-only: no endpoint registration, no property write, no Trouter
connection. Apart from the slip noted in item 9, values printed during the checks were limited to
status codes, key names, counts, enum values, scope names and service URLs.
