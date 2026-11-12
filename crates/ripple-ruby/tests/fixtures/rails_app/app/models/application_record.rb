# Superclass ref spans `ActiveRecord::Base`; ActiveRecord is reopened only in
# lib/active_record/batches.rb, which must NOT receive an edge.
class ApplicationRecord < ActiveRecord::Base
  self.abstract_class = true
end
