module Admin
  class Profile < ApplicationRecord
    belongs_to :account
    has_many :people
  end
end
