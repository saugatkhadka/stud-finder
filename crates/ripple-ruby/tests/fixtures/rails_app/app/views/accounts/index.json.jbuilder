json.array! @accounts do |a|
  json.partial! "accounts/card", account: a
  json.visibility Status::VISIBILITY
end
