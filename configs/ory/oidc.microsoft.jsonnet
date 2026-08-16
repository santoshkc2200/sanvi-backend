// Microsoft OIDC → Sanvi traits. Microsoft only issues emails it has
// verified, and returns no email_verified claim — the mapper marks the
// address verified exactly when the claim exists.
function(ctx) {
  identity: {
    traits: {
      email: ctx.claims.email,
      name: {
        // `name` is the display name ("Last, First" locally): split
        // defensively; the settings flow is the fallback for correction.
        first: if std.objectHas(ctx.claims, "given_name") then ctx.claims.given_name
          else std.split(ctx.claims.name, ", ")[0],
        last: if std.objectHas(ctx.claims, "family_name") then ctx.claims.family_name
          else if std.length(std.split(ctx.claims.name, ", ")) > 1
            then std.split(ctx.claims.name, ", ")[1]
          else "",
      },
      locale: if std.objectHas(ctx.claims, "locale") then ctx.claims.locale else "",
    },
    verifiable_addresses: [
      if std.objectHas(ctx.claims, "email") then
        { value: ctx.claims.email, via: "email", verified: true, status: "completed" }
      else {},
    ],
  },
}
