class Status < ApplicationRecord
  VISIBILITY = %i[public private].freeze
  belongs_to :account
  has_many :media_attachments
end
