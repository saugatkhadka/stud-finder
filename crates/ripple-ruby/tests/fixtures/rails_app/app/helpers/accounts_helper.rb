module AccountsHelper
  def badge_label(account)
    account.name
  end

  def shared_title
    "x"
  end

  def self.not_a_view_helper; end
end
