-- lua handler in the auth chain: returns rate attributes the BNG understands, according to the user's plan.
-- The sql handler before it has already checked the password; all_accept stops at a reject, so a user with a wrong password never gets here.
--
-- BNGs from different vendors need different attributes for the same plan. Each BNG is a RADIUS client, and the nas_styles table says which
-- style it uses; a client not listed uses profile. A style is an entry in the styles table below; to support another vendor, add an entry:
--   - Name-only styles (profile, alc_sla, erx): the rate is set on the BNG in the profile found by name, so the unit and direction are not here.
--   - Value styles (wispr, mikrotik): the unit and direction come from the attribute's definition. Check them against your BNG's documentation.
-- Download (down) is toward the user and upload (up) is from the user toward the network; the plans' rates are in kbit/s.

local DEFAULT_STYLE = 'profile'
local INTERIM_SECONDS = 300

local styles = {
  -- Standard Filter-Id: the value is a profile name on the BNG.
  profile = function(plan)
    set_attr('Filter-Id', plan.profile)
  end,
  -- Wireless ISP Roaming (WISPr): two directions, unit bit/s. The attribute is a 32-bit integer, at most 4294967295 bit/s
  -- (about 4.29 Gbit/s); a faster plan cannot be sent, so reject and log it rather than send a truncated or invalid value.
  wispr = function(plan)
    local down, up = plan.down_kbps * 1000, plan.up_kbps * 1000
    if down > 4294967295 or up > 4294967295 then
      log('rate limit: plan', plan.plan, 'is faster than WISPr-Bandwidth can say (4294967295 bit/s)')
      return false
    end
    set_attr('WISPr-Bandwidth-Max-Down', down)
    set_attr('WISPr-Bandwidth-Max-Up', up)
  end,
  -- Mikrotik-Rate-Limit: "rx/tx", seen from the router: rx is the user upload, tx is the user download; k is kbit/s.
  mikrotik = function(plan)
    set_attr('Mikrotik-Rate-Limit', string.format('%dk/%dk', plan.up_kbps, plan.down_kbps))
  end,
  -- SLA profile name.
  alc_sla = function(plan)
    set_attr('Alc-SLA-Prof-Str', plan.profile)
  end,
  -- Ingress and egress policy names: ingress is the user upload and egress is the user download; the names are the profile name plus -up or -down.
  erx = function(plan)
    set_attr('ERX-Ingress-Policy-Name', plan.profile .. '-up')
    set_attr('ERX-Egress-Policy-Name', plan.profile .. '-down')
  end,
}

if phase ~= 'authentication' then
  return 'accept'
end
if protocol ~= 'radius' then
  return 'reject'
end

local user = auth_user or get_attr('User-Name')
if not user or user == '' then
  return 'reject'
end

local rows = sql_query([[SELECT p.plan, p.down_kbps, p.up_kbps, p.profile
                         FROM subscribers s LEFT JOIN plans p ON p.plan = s.plan
                         WHERE s.username = ? AND s.enabled = 1]], {user})
if #rows ~= 1 then
  log('rate limit: unknown or disabled user', user)
  return 'reject'
end
local plan = rows[1]
if plan.plan == nil then
  log('rate limit: the plan of', user, 'does not exist')
  return 'reject'
end

local choice = sql_query('SELECT style FROM nas_styles WHERE client = ?', {client})
local style = choice[1] and choice[1].style or DEFAULT_STYLE
local apply = styles[style]
if not apply then
  log('rate limit: client', client, 'has the style', style, 'which this script does not know')
  return 'reject'
end

-- A style function returning false means this plan cannot be sent; returning nothing means it was sent.
if apply(plan) == false then
  return 'reject'
end
set_attr('Acct-Interim-Interval', INTERIM_SECONDS)
return 'accept'
