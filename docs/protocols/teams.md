# Microsoft Teams client protocol

Status: sections 1-8 come from static analysis (2026-10-26): no account was signed in for them, no
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
   token for `https://ic3.teams.office.com` - see "Live results", item 3).
6. Exact values for `x-ms-client-type`, `x-ms-client-version` and `clientinfo` that the services
   accept from a non-official client.
7. Trouter allocate response fields, the downstream request envelope and the `/messaging` payload
   (compression, ack requirements), and which PNH app id/template the registrar accepts for a new
   client.
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

All checks in items 1-4 were read-only: no endpoint registration, no property write, no Trouter
connection. Values printed during the checks were limited to status codes, key names, counts and
service URLs.
