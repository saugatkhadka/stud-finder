class PurchasesController < ApplicationController
  before_action :load_account, only: [:show]
  before_action { Status.count }

  def index
    @accounts = Account.all
  end

  def show
    @account = Account.find(params[:id])
    render partial: "shared/nav"
  end

  def edit
    Person.first
    "TagParser".constantize
  end

  def self.export
    Admin::Note.all
  end

  private def secret
    Admin::Profile.first
  end

  private

  def load_account
    Person.new
  end
end
