class AccountsController < ApplicationController
  layout "admin"

  def show
    @account = Account.find(params[:id])
    render partial: "shared/nav"
  end

  def index
    render "missing_template"
  end
end
