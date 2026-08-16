// Kratos → Sanvi webhook body. Kept deliberately small: the API parses
// defensively anyway, but shrinking the payload reduces what a compromised
// flow can hand us and what we store in traces.
//
// NOTE: Kratos strips `credentials` from the webhook context — OIDC
// identifiers are fetched by the API through the Kratos admin API instead
// (see the sync use case). Registration webhooks fire pre-persist too
// (identity.id still zero); the API ignores those and acts on the
// post-persist call.
function(ctx) {
  flow: "login",
  identity: {
    id: ctx.identity.id,
    traits: ctx.identity.traits,
    verifiable_addresses: ctx.identity.verifiable_addresses,
  },
  session: if std.objectHas(ctx, "session") && ctx.session != null then
    { id: ctx.session.id }
  else {},
}
