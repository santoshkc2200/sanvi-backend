// Google OIDC → Sanvi traits. Carries email_verified through explicitly:
// an auto-link is only ever allowed for providers whose verification status
// we can read from the claims.
function(ctx) {
  identity: {
    traits: {
      email: ctx.claims.email,
      name: {
        first: if std.objectHas(ctx.claims, "given_name") then ctx.claims.given_name else "",
        last: if std.objectHas(ctx.claims, "family_name") then ctx.claims.family_name else "",
      },
      locale: if std.objectHas(ctx.claims, "locale") then ctx.claims.locale else "",
    },
    verifiable_addresses: [
      if std.objectHas(ctx.claims, "email_verified") && ctx.claims.email_verified then
        { value: ctx.claims.email, via: "email", verified: true, status: "completed" }
      else
        { value: ctx.claims.email, via: "email", verified: false, status: "pending" },
    ],
  },
}
