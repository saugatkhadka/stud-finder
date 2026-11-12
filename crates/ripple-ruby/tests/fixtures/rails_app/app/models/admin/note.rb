# Compact header: the `Admin` prefix is namespace pass-through, no edge to admin.rb.
class Admin::Note < ApplicationRecord
  has_many :statuses
  belongs_to :profile
end
